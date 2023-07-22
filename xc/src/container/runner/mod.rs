// Copyright (c) 2023 Yan Ka, Chiu.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
// 1. Redistributions of source code must retain the above copyright
//    notice, this list of conditions, and the following disclaimer,
//    without modification, immediately at the beginning of the file.
// 2. The name of the author may not be used to endorse or promote products
//    derived from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS'' AND
// ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE FOR
// ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
// OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
// HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
// LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
// OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
// SUCH DAMAGE.

mod control_stream;

use self::control_stream::{ControlStream, Readiness};

use crate::container::error::ExecError;
use crate::container::process::*;
use crate::container::running::RunningContainer;
use crate::container::{ContainerManifest, ProcessStat};
use crate::elf::{brand_elf_if_unsupported, ElfBrand};
use crate::models::exec::{Jexec, StdioMode};
use crate::models::network::HostEntry;
use crate::util::exists_exec;

use anyhow::Context;
use freebsd::event::{EventFdNotify, KEventExt};
use freebsd::FreeBSDCommandExt;
use ipc::packet::codec::json::JsonPacket;
use jail::process::Jailed;
use nix::libc::intptr_t;
use nix::sys::event::{kevent_ts, EventFilter, EventFlag, FilterFlag, KEvent};
use std::collections::{HashMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch::{channel, Receiver, Sender};
use tracing::{debug, error, info, trace, warn};

#[derive(Debug)]
pub struct ProcessRunnerStat {
    pub(super) id: String,
    pub(super) pid: u32,
    pub(super) process_stat: Sender<ProcessStat>,
    pub(super) notify: Option<Arc<EventFdNotify>>,
}

impl ProcessRunnerStat {
    pub(super) fn pid(&self) -> u32 {
        self.pid
    }
    pub(super) fn id(&self) -> &str {
        self.id.as_str()
    }
    pub(super) fn set_exited(&mut self, exit_code: i32) {
        self.process_stat.send_if_modified(|status| {
            status.set_exited(exit_code);
            true
        });
    }
    pub(super) fn set_tree_exited(&mut self) {
        self.process_stat.send_if_modified(|status| {
            status.set_tree_exited();
            true
        });
        if let Some(notify) = &self.notify {
            notify.clone().notify_waiters();
        }
    }
}

#[derive(Debug)]
pub struct ProcessRunner {
    pub(super) kq: i32,
    pub(super) named_process: Vec<ProcessRunnerStat>,
    pub(super) pmap: HashMap<u32, u32>,
    pub(super) rpmap: HashMap<u32, Vec<u32>>,

    pub(super) control_streams: HashMap<i32, ControlStream>,

    /// This field records the epoch seconds when the container is "started", which defined by a
    /// container that has completed its init-routine
    pub(super) started: Option<u64>,

    /// If `auto_start` is true, the container executes its init routine automatically after
    /// creation
    pub(super) auto_start: bool,

    container: RunningContainer,

    main_started: bool,
    main_exited: bool,

    // a queue containing the processes to be spawn by the end of event loop
    spawn_queue: VecDeque<(String, Jexec)>,

    inits: SerialExec,

    deinits: SerialExec,
}

/// Processes that should start synchronously; that the next process should start if and only if
/// the previous process (but not all the descentdant of previous proces) exited
#[derive(Debug)]
struct SerialExec {
    base_id: String,
    idx: usize,
    execs: VecDeque<Jexec>,
    last_spawn: Option<String>,
    activated: bool,
}

impl SerialExec {
    fn new(base_id: &str, execs: Vec<Jexec>, activated: bool) -> SerialExec {
        SerialExec {
            base_id: base_id.to_string(),
            execs: VecDeque::from(execs),
            idx: 0,
            last_spawn: None,
            activated,
        }
    }

    fn activate(&mut self) {
        self.activated = true;
    }

    fn is_empty(&self) -> bool {
        self.execs.is_empty()
    }

    fn pop_front(&mut self) -> Option<(String, Jexec)> {
        let exec_id = format!("{}.{}", self.base_id, self.idx);
        let exec = self.execs.pop_front()?;
        self.last_spawn = Some(exec_id.to_string());
        self.idx += 1;
        Some((exec_id, exec))
    }

    fn try_drain_proc_queue(
        &mut self,
        id: &str,
        next_processes: &mut VecDeque<(String, Jexec)>,
    ) -> bool {
        if self.activated {
            match &self.last_spawn {
                Some(last_spawn) if last_spawn != id => false,
                Some(last_spawn) if last_spawn == id && self.is_empty() => {
                    info!("{} drained", self.base_id);
                    true
                }
                _ => {
                    if !self.is_empty() {
                        let exec_id = format!("{}.{}", self.base_id, self.idx);
                        let exec = self.execs.pop_front().unwrap();
                        self.last_spawn = Some(exec_id.to_string());
                        next_processes.push_back((exec_id, exec));
                        self.idx += 1;
                    }
                    false
                }
            }
        } else {
            false
        }
    }
}

impl ProcessRunner {
    pub(crate) fn add_control_stream(&mut self, control_stream: ControlStream) {
        debug!("adding control stream");
        let fd = control_stream.socket_fd();
        self.control_streams.insert(fd, control_stream);
        let read_event = KEvent::from_read(fd);
        _ = kevent_ts(self.kq, &[read_event], &mut [], None);
    }

    pub fn trace_process(
        &mut self,
        id: &str,
        pid: u32,
        stat: Sender<ProcessStat>,
        notify: Option<Arc<EventFdNotify>>,
    ) {
        debug!("trace process id: {id}, pid: {pid}");
        let rstat = ProcessRunnerStat {
            pid,
            id: id.to_string(),
            process_stat: stat,
            notify,
        };
        self.named_process.push(rstat);
        self.rpmap.insert(pid, vec![pid]);
        let event = KEvent::from_trace_pid(pid, FilterFlag::NOTE_EXIT);
        _ = kevent_ts(self.kq, &[event], &mut [], None);
    }

    pub fn find_exec(&self, env_path: &str, exec: &str) -> Option<PathBuf> {
        let root = Path::new(&self.container.root).to_path_buf();
        let exec_path = Path::new(&exec);

        if exec_path.is_absolute() {
            let mut path = root.clone();
            for component in exec_path.components() {
                if component != Component::RootDir {
                    path.push(component);
                }
            }
            exists_exec(root, path, 64).unwrap()
        } else {
            env_path
                .split(':')
                .map(|s| s.to_string())
                .find_map(|search_path| {
                    // we are in the host's jail trying to find an executable in child's root tree
                    let root = root.clone();
                    let mut path = root.clone();
                    for component in Path::new(&search_path).components() {
                        if component != Component::RootDir {
                            path.push(component);
                        }
                    }
                    path.push(exec);
                    if let Ok(candidate) = path.canonicalize() {
                        exists_exec(root, candidate, 64).unwrap()
                    } else {
                        trace!("failed to canonicalize {path:?}");
                        None
                    }
                })
        }
    }

    fn spawn_process(
        &mut self,
        id: &str,
        exec: &Jexec,
        notify: Option<Arc<EventFdNotify>>,
    ) -> Result<SpawnInfo, ExecError> {
        debug!("spawn: {exec:#?}");
        let jail = freebsd::jail::RunningJail::from_jid_unchecked(self.container.jid);
        let paths = exec
            .envs
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| "/bin:/usr/bin:/sbin:/usr/sbin".to_string());
        let path = self
            .find_exec(&paths, &exec.arg0)
            .ok_or(ExecError::ExecutableNotFound)?;

        let (tx, rx) = channel(ProcessStat::new(exec.clone()));

        if self.container.linux {
            if !freebsd::exists_kld("linux") && !freebsd::exists_kld("linux64") {
                return Err(ExecError::MissingLinuxKmod);
            }
            brand_elf_if_unsupported(path, ElfBrand::Linux).map_err(ExecError::BrandELFFailed)?;
        }

        let mut cmd = std::process::Command::new(&exec.arg0);

        cmd.env_clear()
            .args(&exec.args)
            .envs(&exec.envs)
            .jail(&jail);

        if let Some(work_dir) = &exec.work_dir {
            cmd.jwork_dir(work_dir);
        }

        let spawn_info_result = match &exec.output_mode {
            StdioMode::Terminal => {
                let socket_path = format!("/var/run/xc.{}.{}", self.container.id, id);
                let log_path = format!("/var/log/xc.{}.{}.log", self.container.id, id);
                spawn_process_pty(cmd, &log_path, &socket_path)
            }
            StdioMode::Files { stdout, stderr } => spawn_process_files(&mut cmd, stdout, stderr),
            StdioMode::Inherit => {
                let out_path = format!("/var/log/xc.{}.{}.out.log", self.container.id, id);
                let err_path = format!("/var/log/xc.{}.{}.err.log", self.container.id, id);
                spawn_process_files(&mut cmd, &Some(out_path), &Some(err_path))
            }
            StdioMode::Forward {
                stdin,
                stdout,
                stderr,
            } => spawn_process_forward(&mut cmd, *stdin, *stdout, *stderr),
        };

        let spawn_info = spawn_info_result.map_err(|error| {
            if let Some(n) = notify.clone() {
                n.notify_waiters();
            }
            error
        })?;

        let pid = spawn_info.pid;

        tx.send_if_modified(|status| {
            status.set_started(spawn_info.clone());
            true
        });

        let rstat = ProcessRunnerStat {
            pid,
            id: id.to_string(),
            process_stat: tx,
            notify,
        };

        self.container.processes.insert(id.to_string(), rx);

        self.named_process.push(rstat);
        self.rpmap.insert(pid, vec![pid]);
        let event = KEvent::from_trace_pid(pid, FilterFlag::NOTE_EXIT);
        _ = kevent_ts(self.kq, &[event], &mut [], None);

        Ok(spawn_info)
    }

    pub fn pid_ancestor(&self, pid: u32) -> u32 {
        let mut pid = pid;
        while let Some(parent) = self.pmap.get(&pid) {
            pid = *parent;
        }
        pid
    }

    pub fn kill(&self) {
        let event = KEvent::new(
            2,
            EventFilter::EVFILT_USER,
            EventFlag::EV_ONESHOT,
            FilterFlag::NOTE_TRIGGER | FilterFlag::NOTE_FFNOP,
            0 as intptr_t,
            0 as intptr_t,
        );
        _ = kevent_ts(self.kq, &[event], &mut [], None);
    }

    pub fn new(kq: i32, container: RunningContainer, auto_start: bool) -> ProcessRunner {
        ProcessRunner {
            kq,
            named_process: Vec::new(),
            pmap: HashMap::new(),
            rpmap: HashMap::new(),
            control_streams: HashMap::new(),
            main_started: false,
            spawn_queue: VecDeque::new(),
            inits: SerialExec::new("init", container.init_proto.clone(), !container.init_norun),
            deinits: SerialExec::new("deinit", container.deinit_proto.clone(), false),
            main_exited: false,
            container,
            started: None,
            auto_start,
        }
    }

    #[inline]
    pub fn run_main(&mut self) {
        if let Some(main) = self.container.main_proto.clone() {
            self.spawn_queue.push_back(("main".to_string(), main));
        }
    }

    fn handle_control_stream_cmd(&mut self, mut fd: i32, method: String, request: JsonPacket) {
        use ipc::proto::write_response;
        use ipc::transport::PacketTransport;
        use std::io::Write;
        if method == "exec" {
            let jexec: Jexec = serde_json::from_value(request.data).unwrap();
            let notify = Arc::new(EventFdNotify::from_fd(jexec.notify.unwrap()));
            let result = self.spawn_process(&crate::util::gen_id(), &jexec, Some(notify));

            match result {
                Ok(spawn_info) => {
                    let packet = write_response(0, spawn_info).unwrap();
                    _ = fd.send_packet(&packet).unwrap();
                }
                Err(_err) => {
                    let packet = write_response(
                        freebsd::libc::EIO,
                        serde_json::json!({
                            "message": "failed to spawn"
                        }),
                    )
                    .unwrap();
                    _ = fd.send_packet(&packet).unwrap();
                }
            }
        } else if method == "run_main" {
            if let Some(main) = self.container.main_proto.clone() {
                self.spawn_queue.push_back(("main".to_string(), main));
            } else {
                let packet = write_response(0, ()).unwrap();
                _ = fd.send_packet(&packet).unwrap()
            }
        } else if method == "start" {
            self.start();
            let packet = write_response(0, ()).unwrap();
            _ = fd.send_packet(&packet).unwrap()
        } else if method == "write_hosts" {
            let recv: Vec<HostEntry> = serde_json::from_value(request.data).unwrap();
            if let Ok(host_path) = crate::util::realpath(&self.container.root, "/etc/hosts") {
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(host_path)
                {
                    _ = writeln!(&mut file, "::1 localhost");
                    _ = writeln!(&mut file, "127.0.0.1 localhost");
                    for entry in recv.iter() {
                        _ = writeln!(&mut file, "{} {}", entry.ip_addr, entry.hostname);
                    }
                }
            }
            let res_packet = write_response(0, ()).unwrap();
            _ = fd.send_packet(&res_packet).unwrap()
        }
    }

    fn start(&mut self) {
        if self.started.is_none() {
            self.started = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            );
            if self.inits.is_empty() && !self.container.main_norun {
                self.run_main();
            } else if let Some((id, jexec)) = self.inits.pop_front() {
                self.inits.activate();
                _ = self.spawn_process(&id, &jexec, None);
            }
        } else {
            error!("self.start() is called but the container has already started!")
        }
    }

    fn handle_pid_event(&mut self, event: KEvent, last_deinit: &mut Option<String>) -> bool {
        let fflag = event.fflags();
        let pid = event.ident() as u32;
        if fflag.contains(FilterFlag::NOTE_EXIT) {
            let ancestor = self.pid_ancestor(pid);
            self.pmap.remove(&pid);

            let descentdant = self.rpmap.get_mut(&ancestor).unwrap();
            trace!(
                pid,
                ancestor,
                "NOTE_EXIT: {pid} exited; ancestor: {ancestor}"
            );

            if let Some(pos) = descentdant.iter().position(|x| *x == pid) {
                descentdant.remove(pos);
            }
            let descentdant_gone = descentdant.is_empty();
            if descentdant_gone {
                debug!("all descentdant of pid {ancestor} are gone");
            }

            if ancestor == pid || descentdant_gone {
                for stat in self.named_process.iter_mut() {
                    if stat.pid() == ancestor {
                        if ancestor == pid {
                            stat.set_exited(event.data() as i32);
                            unsafe { nix::libc::waitpid(pid as i32, std::ptr::null_mut(), 0) };

                            if self
                                .inits
                                .try_drain_proc_queue(stat.id(), &mut self.spawn_queue)
                                && !self.container.main_norun
                            {
                                if let Some(main) = self.container.main_proto.clone() {
                                    self.spawn_queue.push_back(("main".to_string(), main));
                                }
                            }

                            if self
                                .deinits
                                .try_drain_proc_queue(stat.id(), &mut self.spawn_queue)
                            {
                                *last_deinit = self.deinits.last_spawn.clone();
                                // allow for the last deinit action to run at most
                                // 15 seconds
                                let event = KEvent::from_timer_seconds_oneshot(1486, 15);
                                _ = kevent_ts(self.kq, &[event], &mut [], None);
                            }
                        }
                        if descentdant_gone {
                            stat.set_tree_exited();
                            if stat.id() == "main" {
                                self.main_exited = true;
                                if (self.container.deinit_norun || self.deinits.is_empty())
                                    && !self.container.persist
                                {
                                    return true;
                                } else {
                                    debug!("activating deinit queue");
                                    self.deinits.activate();
                                    self.deinits.try_drain_proc_queue("", &mut self.spawn_queue);
                                }
                            } else if let Some(last_deinit) = last_deinit.clone() {
                                if last_deinit == stat.id() {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        } else if fflag.contains(FilterFlag::NOTE_CHILD) {
            let parent = event.data() as u32;
            let ancestor = self.pid_ancestor(parent);
            trace!("NOTE_CHILD: parent {parent}, pid {pid}, ancestor: {ancestor}");
            self.pmap.insert(pid, ancestor);
            let v = self.rpmap.get_mut(&ancestor).expect("cannot find ancestor");
            v.push(pid);
        }

        false
    }

    pub fn run(mut self, sender: Sender<ContainerManifest>) {
        let mut events = vec![KEvent::zero(); 64];
        let kq = self.kq;
        let kill_event = KEvent::new(
            2,
            EventFilter::EVFILT_USER,
            EventFlag::EV_ONESHOT | EventFlag::EV_ADD | EventFlag::EV_ENABLE,
            FilterFlag::NOTE_FFNOP,
            0 as intptr_t,
            0 as intptr_t,
        );

        _ = kevent_ts(kq, &[kill_event], &mut [], None);

        let mut last_deinit = None;

        if self.auto_start {
            self.start();
        }

        'kq: loop {
            while let Some((id, process)) = self.spawn_queue.pop_front() {
                match self.spawn_process(&id, &process, None) {
                    Ok(spawn_info) => {
                        debug!("{id} spawn: {spawn_info:#?}");
                        if id == "main" {
                            self.main_started = true;
                            self.container.main_started_notify.notify_waiters();
                        }
                    }
                    Err(error) => error!("cannot spawn {id}: {error:#?}"),
                }
            }

            sender.send_if_modified(|x| {
                *x = self.container.serialized();
                true
            });
            let nevx = kevent_ts(kq, &[], &mut events, None);
            let nev = nevx.unwrap();

            for event in &events[..nev] {
                match event.filter().unwrap() {
                    EventFilter::EVFILT_PROC => {
                        if self.handle_pid_event(*event, &mut last_deinit) {
                            break 'kq;
                        }
                    }
                    EventFilter::EVFILT_TIMER => {
                        // the only timer event is the killer event
                        warn!("deinit time out reached, proceed to kill jail");
                        break 'kq;
                    }
                    EventFilter::EVFILT_READ => {
                        let fd = event.ident() as i32;
                        if event.data() == 0 {
                            self.control_streams.remove(&fd);
                        } else if let Some(control_stream) = self.control_streams.get_mut(&fd) {
                            match control_stream.try_get_request(event.data() as usize) {
                                Err(_) => {
                                    self.control_streams.remove(&fd);
                                }
                                Ok(Readiness::Pending) => {}
                                Ok(Readiness::Ready((method, request))) => {
                                    self.handle_control_stream_cmd(fd, method, request);
                                }
                            }
                        }
                    }
                    EventFilter::EVFILT_USER => {
                        debug!("{event:#?}");
                        if self.container.deinit_norun || self.deinits.is_empty() {
                            break 'kq;
                        } else {
                            debug!("activating deinit queue");
                            self.deinits.activate();
                            self.deinits.try_drain_proc_queue("", &mut self.spawn_queue);
                        }
                    }
                    _ => {
                        debug!("{event:#?}");
                    }
                }
            }
        }

        self.cleanup(sender);
    }

    fn cleanup(&mut self, sender: Sender<ContainerManifest>) {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        info!("cleaning up at {epoch:#?}");
        self.container.destroyed = Some(epoch.as_secs());
        sender.send_if_modified(|x| {
            *x = self.container.serialized();
            true
        });

        let jail = freebsd::jail::RunningJail::from_jid_unchecked(self.container.jid);
        let kill = jail.kill().context("cannot kill jail").map_err(|e| {
            error!("cannot kill jail: {e}");
            e
        });

        info!("jail kill: {kill:#?}");
        // allow 5 seconds for the jail to be killed
        //            std::thread::sleep(std::time::Duration::from_secs(5));
        self.container.notify.notify_waiters();
    }
}

pub fn run(
    container: RunningContainer,
    control_stream: UnixStream,
    auto_start: bool,
) -> (i32, Receiver<ContainerManifest>) {
    let kq = nix::sys::event::kqueue().unwrap();
    let (tx, rx) = channel(container.serialized());
    let mut pr = ProcessRunner::new(kq, container, auto_start);
    pr.add_control_stream(ControlStream::new(control_stream));
    let kq = pr.kq;
    std::thread::spawn(move || {
        pr.run(tx);
    });
    (kq, rx)
}
