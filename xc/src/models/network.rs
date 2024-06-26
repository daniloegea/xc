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
use crate::util::mk_string;
use ipcidr::IpCidr;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Policy to generate /etc/resolv.conf
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DnsSetting {
    Inherit,
    Specified {
        servers: Vec<String>,
        search_domains: Vec<String>,
    },
    Nop,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IpAssign {
    pub network: Option<String>,
    pub addresses: Vec<IpCidr>,
    pub interface: String,
}

impl std::fmt::Display for IpAssign {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(network) = &self.network {
            write!(formatter, "({network})")?;
        }
        write!(formatter, "{}|", self.interface)?;
        let mut once = false;
        for address in self.addresses.iter() {
            if once {
                write!(formatter, ",")?;
            } else {
                once = true;
            }
            address.fmt(formatter)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, std::hash::Hash)]
pub enum PortNum {
    Single(u16),
    /// Represents a range of port number by a starting number and length
    Range(u16, u16),
}

impl std::fmt::Display for PortNum {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single(port) => write!(formatter, "{port}"),
            Self::Range(start, len) => write!(formatter, "{start}:{len}"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, std::hash::Hash)]
pub enum NetProto {
    Tcp,
    Udp,
    Sctp,
}

impl std::fmt::Display for NetProto {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(formatter, "tcp"),
            Self::Udp => write!(formatter, "udp"),
            Self::Sctp => write!(formatter, "sctp"),
        }
    }
}

impl AsRef<str> for NetProto {
    fn as_ref(&self) -> &str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Sctp => "sctp",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortRedirection {
    pub ifaces: Option<Vec<String>>,
    pub proto: Vec<NetProto>,
    /// Where the packet from (source address of the packets)
    pub origin: Vec<String>,
    /// Local addresses (host address) to trigger this redirection
    pub addresses: Option<Vec<IpCidr>>,
    pub source_port: PortNum,
    pub dest_port: u16,
    pub dest_addr: Option<IpCidr>,
}

impl PortRedirection {
    pub fn to_pf_rule(&self) -> String {
        let ifaces = self.ifaces.as_ref().unwrap();
        let protos = mk_string(&self.proto, "{", ",", "}");
        let ext_ifs = mk_string(ifaces, "{", ",", "}");
        let dest_addr = self.dest_addr.as_ref().unwrap();
        let source = match &self.addresses {
            None => "any".to_string(),
            Some(addresses) => {
                let addresses = addresses
                    .iter()
                    .map(|addr| addr.to_singleton().to_string())
                    .collect::<Vec<_>>();
                mk_string(&addresses, "{", ",", "}")
            }
        };
        format!(
            "rdr on {ext_ifs} proto {protos} from any to {source} port {} -> {} port {}",
            self.source_port,
            dest_addr.to_singleton(),
            self.dest_port
        )
    }

    pub fn with_host_info(&mut self, ext_ifaces: &[String], main_ip: IpCidr) {
        if self.ifaces.is_none() {
            self.ifaces = Some(ext_ifaces.to_vec());
        }
        self.dest_addr = Some(main_ip)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostEntry {
    pub ip_addr: IpAddr,
    pub hostname: String,
}

#[derive(Clone, Debug)]
pub enum MainAddressSelector {
    Network(String),
    Ip(IpAddr),
}

pub struct AssignedAddress {
    pub interface: String,
    pub address: IpAddr,
}

impl AssignedAddress {
    pub fn new(interface: String, address: IpAddr) -> AssignedAddress {
        AssignedAddress { interface, address }
    }
}

impl MainAddressSelector {
    pub fn select<'a, I: Iterator<Item = &'a IpAssign>>(
        selector: &Option<Self>,
        pool: I,
    ) -> Option<AssignedAddress> {
        match selector {
            None => {
                for alloc in pool {
                    if alloc.network.is_some() {
                        if let Some(address) = alloc.addresses.first() {
                            return Some(AssignedAddress::new(
                                alloc.interface.to_string(),
                                address.addr(),
                            ));
                        }
                    }
                }
                None
            }
            Some(MainAddressSelector::Ip(address)) =>
            /*Some(*address)*/
            {
                for alloc in pool {
                    if alloc.addresses.iter().any(|addr| addr.addr() == *address) {
                        return Some(AssignedAddress::new(alloc.interface.to_string(), *address));
                    }
                }
                None
            }
            Some(MainAddressSelector::Network(network)) => {
                for alloc in pool {
                    match alloc.network.as_ref() {
                        Some(_network) if network == _network => match alloc.addresses.first() {
                            None => continue,
                            Some(addr) => {
                                return Some(AssignedAddress::new(
                                    alloc.interface.to_string(),
                                    addr.addr(),
                                ))
                            }
                        },
                        _ => continue,
                    }
                }
                None
            }
        }
    }
}

impl std::fmt::Display for MainAddressSelector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MainAddressSelector::Ip(ip) => formatter.write_fmt(format_args!("ipadd/{ip}")),
            MainAddressSelector::Network(network) => {
                formatter.write_fmt(format_args!("network/{network}"))
            }
        }
    }
}

impl std::str::FromStr for MainAddressSelector {
    type Err = anyhow::Error;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        input
            .split_once('/')
            .ok_or(anyhow::anyhow!("malformed"))
            .and_then(|(proto, val)| match proto {
                "ipaddr" => val
                    .parse::<IpAddr>()
                    .map_err(anyhow::Error::new)
                    .map(MainAddressSelector::Ip),
                "network" => Ok(MainAddressSelector::Network(val.to_string())),
                _ => Err(anyhow::anyhow!("malformed")),
            })
    }
}

impl Serialize for MainAddressSelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

struct MainAddressSelectorVisitor;

impl<'de> serde::de::Visitor<'de> for MainAddressSelectorVisitor {
    type Value = MainAddressSelector;
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("expecting ip/<ipadr> or network/<network-name>")
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        value.parse().map_err(|e| E::custom(format!("{e:?}")))
    }
}

impl<'de> Deserialize<'de> for MainAddressSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_str(MainAddressSelectorVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{NetProto, PortNum, PortRedirection};

    #[test]
    fn test_generated_pf_rdr_rule() {
        let mut rdr = PortRedirection {
            ifaces: None,
            proto: vec![NetProto::Tcp, NetProto::Udp],
            origin: Vec::new(),
            addresses: None,
            source_port: PortNum::Single(22),
            dest_port: 88,
            dest_addr: None,
        };

        rdr.with_host_info(&["cxl0".to_string()], "192.168.1.1/24".parse().unwrap());

        let rule = rdr.to_pf_rule();

        assert_eq!(
            &rule,
            "rdr on {cxl0} proto {tcp,udp} from any to any port 22 -> 192.168.1.1/32 port 88"
        );
    }
}
