/*********************************************************************************
* Copyright (c) 2018,2021 ADLINK Technology Inc.
*
* This program and the accompanying materials are made available under the
* terms of the Eclipse Public License 2.0 which is available at
* http://www.eclipse.org/legal/epl-2.0, or the Apache Software License 2.0
* which is available at https://www.apache.org/licenses/LICENSE-2.0.
*
* SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
* Contributors:
*   ADLINK fog05 team, <fog05@adlink-labs.tech>
*********************************************************************************/
#![allow(unused)]
#![allow(clippy::too_many_arguments)]
extern crate tera;

use std::collections::HashMap;
use std::convert::From;
use std::error::Error;
use std::ffi::{self, CString};
use std::os::unix::io::IntoRawFd;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use async_std::prelude::*;
use async_std::sync::{Arc, RwLock};
use async_std::task;

use log::{error, info, trace};

use znrpc_macros::znserver;
use zrpc::ZNServe;

use zenoh::*;

use fog05_sdk::agent::{os::OSClient, plugin::AgentPluginInterfaceClient};
use fog05_sdk::fresult::{FError, FResult};
use fog05_sdk::plugins::networking::NetworkingPlugin;
use fog05_sdk::types::{
    BridgeKind, ConnectionPoint, GREKind, IPAddress, IPConfiguration, IPVersion, Interface,
    InterfaceKind, LinkKind, MACAddress, MACVLANKind, MCastVXLANInfo, NetworkNamespace,
    P2PVXLANInfo, PluginKind, VETHKind, VLANKind, VXLANKind, VirtualInterface,
    VirtualInterfaceConfig, VirtualInterfaceConfigKind, VirtualInterfaceKind, VirtualNetwork,
};

use uuid::Uuid;

use futures::stream::TryStreamExt;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

use netlink_packet_route::rtnl::address::nlas::Nla;
use rtnetlink::Error as nlError;
use rtnetlink::NetworkNamespace as NetlinkNetworkNamespace;
use rtnetlink::{new_connection, Handle};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use ipnetwork::IpNetwork;

use nftnl::{nft_expr, nftnl_sys::libc, Batch, Chain, FinalizedBatch, ProtoFamily, Rule, Table};

use tera::{Context, Result, Tera};

use crate::types::{
    deserialize_network_internals, serialize_network_internals, LinuxNetwork, LinuxNetworkConfig,
    LinuxNetworkState, LinuxNetworkStateGuard, NamespaceManagerClient, VNetDHCP, VNetNetns,
    VirtualNetworkInternals,
};

#[znserver]
impl NetworkingPlugin for LinuxNetwork {
    /// Creates the default fosbr0 virtual network
    /// it's UUID is 00000000-0000-0000-0000-000000000000
    /// it is a VXLAN kind of virtual network
    /// VNI: 3845
    /// MCast Addr: 239.15.5.0
    /// Port 3845
    /// Net: 10.240.0.0/16
    /// Gateway: 10.240.0.1
    /// Agents checks if there is already a default network in the system
    /// if so it calls with the DHCP set to false
    /// otherwise it is set to true an a DHCP for the default network
    /// is started in the node
    async fn create_default_virtual_network(&self, dhcp: bool) -> FResult<VirtualNetwork> {
        log::debug!(
            "entering create_default_virtual_network with dhcp: {}",
            dhcp
        );

        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let default_net_uuid = Uuid::nil();

        let default_br_uuid = Uuid::nil();
        let default_br_name = String::from("fosbr0");

        let default_vxl_uuid = Uuid::new_v4();
        let default_vxl_name = String::from("fosvxl0");

        // let default_netns_uuid = Uuid::nil();
        // let default_netns_name = String::from("fos-default");

        // let default_veth_i_uuid = Uuid::new_v4();
        // let default_veth_i_name = String::from("fveth-default-i");

        // let default_veth_e_uuid = Uuid::new_v4();
        // let default_veth_e_name = String::from("fveth-default-e");

        let default_vni: u32 = 3845;
        let default_mcast_addr = IPAddress::V4(std::net::Ipv4Addr::new(239, 15, 5, 0));
        let default_port: u16 = 3845;

        let dafault_ext_if_name = self.get_overlay_iface().await?;

        let mut default_vnet = VirtualNetwork {
            uuid: default_net_uuid,
            id: String::from("fos-default"),
            name: Some(String::from("Eclipse fog05 default virtual network")),
            is_mgmt: false,
            link_kind: LinkKind::L2(MCastVXLANInfo {
                vni: default_vni,
                mcast_addr: default_mcast_addr,
                port: default_port,
            }),
            ip_version: IPVersion::IPV4,
            ip_configuration: None,
            connection_points: Vec::new(),
            interfaces: vec![
                default_br_uuid,
                default_vxl_uuid,
                // default_veth_i_uuid,
                // default_veth_e_uuid,
            ],
            plugin_internals: None,
        };

        // let mut default_netns = NetworkNamespace {
        //     uuid: default_netns_uuid,
        //     ns_name: String::from("fos-default"),
        //     interfaces: vec![default_veth_e_uuid],
        // };

        if dhcp {
            let ip_conf = IPConfiguration {
                subnet: Some((IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 0, 0)), 16)),
                gateway: Some(IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 0, 1))),
                dhcp_range: Some((
                    IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 0, 2)),
                    IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 255, 254)),
                )),
                dns: Some(vec![
                    IPAddress::V4(std::net::Ipv4Addr::new(208, 67, 222, 222)),
                    // IPAddress::V4(std::net::Ipv4Addr::new(208, 67, 222, 220)),
                ]),
            };
            default_vnet.ip_configuration = Some(ip_conf);
        }

        let v_bridge = VirtualInterface {
            uuid: default_br_uuid,
            if_name: default_br_name.clone(),
            net_ns: None,
            parent: None,
            kind: VirtualInterfaceKind::BRIDGE(BridgeKind {
                childs: vec![default_vxl_uuid],
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let res = self.create_bridge(default_br_name.clone()).await?;
        log::trace!("Bridge creation res: {:?}", res);
        self.set_iface_up(default_br_name.clone()).await?;

        let v_vxl = VirtualInterface {
            uuid: default_vxl_uuid,
            if_name: default_vxl_name.clone(),
            net_ns: None,
            parent: Some(default_br_uuid),
            kind: VirtualInterfaceKind::VXLAN(VXLANKind {
                vni: default_vni,
                mcast_addr: default_mcast_addr,
                port: default_port,
                dev: Interface {
                    if_name: dafault_ext_if_name.clone(),
                    kind: InterfaceKind::ETHERNET,
                    addresses: Vec::new(),
                    phy_address: None,
                },
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let res = self
            .create_mcast_vxlan(
                default_vxl_name.clone(),
                dafault_ext_if_name.clone(),
                default_vni,
                default_mcast_addr,
                default_port,
            )
            .await?;

        log::trace!("VXLAN creation res: {:?}", res);
        // Setting master for VXLAN interface and setting interface up
        self.set_iface_master(default_vxl_name.clone(), default_br_name.clone())
            .await?;
        self.set_iface_up(default_vxl_name).await?;

        // Adding address to bridge interface
        self.add_iface_address(
            default_br_name.clone(),
            IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 0, 1)),
            16,
        )
        .await?;

        // Creating dnsmasq config
        let dhcp_internal = if dhcp {
            let lease_file_path = self
                .get_run_path()
                .join("fosbr0.leases")
                .to_str()
                .ok_or(FError::EncodingError)?
                .to_string();
            let pid_file_path = self
                .get_run_path()
                .join("fosbr0.pid")
                .to_str()
                .ok_or(FError::EncodingError)?
                .to_string();
            let log_file_path = self
                .get_run_path()
                .join("fosbr0.log")
                .to_str()
                .ok_or(FError::EncodingError)?
                .to_string();
            let conf_file_path = self
                .get_run_path()
                .join("fosbr0.conf")
                .to_str()
                .ok_or(FError::EncodingError)?
                .to_string();

            let config = self
                .create_dnsmasq_config(
                    &default_br_name,
                    &pid_file_path,
                    &lease_file_path,
                    &log_file_path,
                    IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 0, 2)),
                    IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 255, 254)),
                    IPAddress::V4(std::net::Ipv4Addr::new(10, 240, 0, 1)),
                    IPAddress::V4(std::net::Ipv4Addr::new(208, 67, 222, 222)),
                )
                .await?;
            log::trace!("dnsmasq config: {}", config);
            self.os
                .as_ref()
                .unwrap()
                .store_file(config.into_bytes(), conf_file_path.clone())
                .await??;
            let child = self.spawn_dnsmasq(conf_file_path.clone()).await?;
            log::debug!("DHCP Process running PID: {}", child.id());
            Some(VNetDHCP {
                leases_file: lease_file_path,
                pid_file: pid_file_path,
                conf: conf_file_path,
                log_file: log_file_path,
            })
        } else {
            None
        };

        // let v_veth_i = VirtualInterface {
        //     uuid: default_veth_i_uuid,
        //     if_name: default_veth_i_name.clone(),
        //     net_ns: None,
        //     parent: Some(default_br_uuid),
        //     kind: VirtualInterfaceKind::VETH(VETHKind {
        //         pair: default_veth_e_uuid,
        //         internal: true,
        //     }),
        //     addresses: Vec::new(),
        //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        // };

        // let v_veth_e = VirtualInterface {
        //     uuid: default_veth_e_uuid,
        //     if_name: default_veth_e_name.clone(),
        //     net_ns: Some(default_netns_uuid),
        //     parent: None,
        //     kind: VirtualInterfaceKind::VETH(VETHKind {
        //         pair: default_veth_i_uuid,
        //         internal: false,
        //     }),
        //     addresses: Vec::new(),
        //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        // };

        // let res = self
        //     .create_veth(default_veth_i_name.clone(), default_veth_e_name.clone())
        //     .await?;
        // log::trace!("VEth Pair creation res: {:?}", res);

        // self.set_iface_master(default_veth_i_name.clone(), default_br_name.clone())
        //     .await?;
        // self.set_iface_up(default_veth_i_name).await?;

        // let res = self.add_netns(default_netns_name.clone()).await?;
        // log::trace!("Netns creation res: {:?}", res);

        // Here we spawn the manager for the just created Namespace and
        // we add it to the map of managers
        // let mut guard = self.state.write().await;
        // let child = Command::new("fos-net-linux-ns-manager")
        //     .arg("--netns")
        //     .arg(&default_netns_name)
        //     .arg("--id")
        //     .arg(format!("{}", default_netns_uuid))
        //     .arg("--locator")
        //     .arg("unixsock-stream//tmp/zenoh.sock")
        //     .spawn()
        //     .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
        // let ns_manager_client = NamespaceManagerClient::new(self.z.clone(), default_netns_uuid);
        // guard
        //     .ns_managers
        //     .insert(default_netns_uuid, (child.id(), ns_manager_client));
        // drop(guard);

        // let res = self.set_iface_up(default_veth_e_name.clone()).await?;
        // log::trace!("veth ext face up res: {:?}", res);
        // let res = self
        //     .set_iface_ns(default_veth_e_name.clone(), default_netns_name.clone())
        //     .await?;
        // log::trace!("veth ext netns set res: {:?}", res);

        // Setting the firewall to NAT
        // using nftables so add as dependencies:
        // nftables libnftnl-dev libnfnetlink-dev libmnl-dev
        // rule is similar to
        // table ip nat { # handle 3
        // 	chain postrouting { # handle 1
        // 		type nat hook postrouting priority srcnat; policy accept;
        // 		ip saddr 10.240.0.0/16 oif "eno0" masquerade # handle 4
        // 	}
        // }
        let nat_table = self
            .configure_nat(
                IpNetwork::V4(
                    ipnetwork::Ipv4Network::new(std::net::Ipv4Addr::new(10, 240, 0, 0), 16)
                        .map_err(|e| FError::NetworkingError(format!("{}", e)))?,
                ),
                &self.get_overlay_face_from_config().await?.if_name,
            )
            .await?;

        self.connector.local.add_interface(&v_bridge).await?;

        self.connector.local.add_interface(&v_vxl).await?;

        let internals = VirtualNetworkInternals {
            // associated_netns_name: default_netns_name,
            associated_netns: None,
            dhcp: dhcp_internal,
            associated_tables: vec![nat_table],
        };

        default_vnet.plugin_internals = Some(serialize_network_internals(&internals)?);

        self.connector
            .local
            .add_virutal_network(&default_vnet)
            .await?;

        log::debug!(
            "leaving create_default_virtual_network with res: {:?}",
            default_vnet
        );
        Ok(default_vnet)
    }

    /// Creates the given virtual network in the current node.
    /// This function is called by the Agent prior to instantiate any FDU
    /// that requires this virtual network.
    /// Network creation means:
    /// 1 - Creation of a virtual bridge representing the network
    /// 2 - Creation of the overlay/vlan face needed for the network
    /// 3 - Creation on DHCP configuration (if any)
    /// 4 - Configuration of netfilter for routing (NAT)
    /// 5 - Creation of the associated network namespace
    /// 6 - Creation of a bridge inside the associated network namespace
    /// 7 - Creation and linking of a veth pair between internal bridge and external bridge
    /// 8 - Link of the overlay/vlan face to the external bridge
    ///
    /// Eg. For a virtual network interfaces will be configured as follow
    ///
    ///
    ///  +--------------------------------------+
    ///  |   Default Node network namespace     |
    ///  |                                      |
    ///  | +---------------------------+        |
    ///  | |  Virtual Network Bridge   |        |
    ///  | |  +----------------+       | +---------------+
    ///  | |  | overlay iface  <---------> eth0 |        |
    ///  | |  +----------------+       | +---------------+
    ///  | |  +----------------+       |        |
    ///  | |  | veth-ext       |       |        |
    ///  | |  +--------^-------+       |        |
    ///  | +-----------|---------------+        |
    ///  |             |                        |
    ///  +-------------|------------------------+
    ///                |
    ///                |
    ///                |
    ///  +-------------|------------------------+
    ///  |  +----------|---------------------+  |
    ///  |  |  +-------v------+ +-----------+|  |
    ///  |  |  |  veth-int    | | fdu-ifacen||  |
    ///  |  |  +--------------+ +-----------+|  |
    ///  |  |  +--------------+              |  |
    ///  |  |  | fdu-iface1   |              |  |
    ///  |  |  +--------------+              |  |
    ///  |  |                                |  |
    ///  |  |                                |  |
    ///  |  |     Internal bridge            |  |
    ///  |  +--------------------------------+  |
    ///  |  Virtual Network network namespace   |
    ///  +--------------------------------------+
    ///
    async fn create_virtual_network(&self, vnet_uuid: Uuid) -> FResult<VirtualNetwork> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match self.connector.global.get_virtual_network(vnet_uuid).await {
            Ok(mut vnet) => {
                if let Ok(net) = self.connector.local.get_virtual_network(vnet_uuid).await {
                    return Ok(net);
                }
                match vnet.clone().link_kind {
                    LinkKind::L2(link_kind_info) => {
                        //Multicast-based VxLAN
                        let vnet = self.mcast_vxlan_create(vnet, link_kind_info).await?;
                        self.connector.local.add_virutal_network(&vnet).await?;
                        Ok(vnet)
                    }
                    LinkKind::ELINE(link_kind_info) => {
                        //P2P-based VxLAN
                        let vnet = self.ptp_vxlan_create(vnet, link_kind_info).await?;
                        self.connector.local.add_virutal_network(&vnet).await?;
                        Ok(vnet)
                    }
                    // Unimplemented for other virtual networks kinds
                    _ => Err(FError::Unimplemented),
                }
            }
            Err(FError::NotFound) => {
                // a virtual network with this UUID does not exists
                Err(FError::NotFound)
            }
            Err(err) => {
                //any other error just return the error
                Err(err)
            }
        }
    }

    async fn get_virtual_network(&self, vnet_uuid: Uuid) -> FResult<VirtualNetwork> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        self.connector.local.get_virtual_network(vnet_uuid).await
    }

    async fn delete_virtual_network(&self, vnet_uuid: Uuid) -> FResult<VirtualNetwork> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match self.connector.local.get_virtual_network(vnet_uuid).await {
            Err(_) => Err(FError::NotFound),
            Ok(vnet) => {
                // if !vnet.interfaces.is_empty() {
                //     return Err(FError::NetworkingError(
                //         "Cannot remove virtual network that has attached interfaces".into(),
                //     ));
                // }
                for i in &vnet.interfaces {
                    log::info!(
                        "Deleting virtual interface: {:?}",
                        self.delete_virtual_interface(*i).await?
                    );
                }

                if !vnet.connection_points.is_empty() {
                    return Err(FError::NetworkingError(
                        "Cannot remove virtual network that has attached connection points".into(),
                    ));
                }

                if let Some(ref pl_net_info) = vnet.plugin_internals {
                    let net_info = deserialize_network_internals(pl_net_info)?;
                    if let Some(ns_info) = net_info.associated_netns {
                        self.delete_network_namespace(ns_info.ns_uuid).await?;
                    }
                }

                self.connector
                    .local
                    .remove_virtual_network(vnet_uuid)
                    .await?;
                Ok(vnet)
            }
        }
    }

    async fn create_connection_point(&self) -> FResult<ConnectionPoint> {
        Err(FError::Unimplemented)
        // let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        // let cp_uuid = Uuid::new_v4();
        // match self
        //     .connector
        //     .local
        //     .get_connection_point(cp_uuid)
        //     .await
        // {
        //     Err(_) => {
        //         let cp = ConnectionPoint {
        //             uuid: cp_uuid,
        //             net_ns: Uuid::new_v4(),
        //             bridge: Uuid::new_v4(),
        //             internal_veth: Uuid::new_v4(),
        //             external_veth: Uuid::new_v4(),
        //         };
        //         self.connector
        //             .local
        //             .add_connection_point(&cp)
        //             .await?;
        //         Ok(cp)
        //     }
        //     Ok(_) => Err(FError::AlreadyPresent),
        // }
    }

    async fn get_connection_point(&self, cp_uuid: Uuid) -> FResult<ConnectionPoint> {
        Err(FError::Unimplemented)
        // let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        // self.connector
        //     .local
        //     .get_connection_point(cp_uuid)
        //     .await
    }

    async fn delete_connection_point(&self, cp_uuid: Uuid) -> FResult<Uuid> {
        Err(FError::Unimplemented)
        // let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        // match self
        //     .connector
        //     .local
        //     .get_connection_point(cp_uuid)
        //     .await
        // {
        //     Err(_) => Err(FError::NotFound),
        //     Ok(_) => {
        //         self.connector
        //             .local
        //             .remove_connection_point(cp_uuid)
        //             .await?;
        //         Ok(cp_uuid)
        //     }
        // }
    }

    async fn create_virtual_interface(
        &self,
        intf: VirtualInterfaceConfig,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match intf.kind {
            VirtualInterfaceConfigKind::VXLAN(conf) => {
                let ext_face = self.get_overlay_face_from_config().await?;
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name.clone(),
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::VXLAN(VXLANKind {
                        vni: conf.vni,
                        mcast_addr: conf.mcast_addr,
                        port: conf.port,
                        dev: ext_face.clone(),
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };

                self.create_mcast_vxlan(
                    intf.if_name,
                    ext_face.if_name.clone(),
                    conf.vni,
                    conf.mcast_addr,
                    conf.port,
                )
                .await?;

                self.connector.local.add_interface(&v_iface).await?;
                Ok(v_iface)
            }
            VirtualInterfaceConfigKind::BRIDGE => {
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name.clone(),
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::BRIDGE(BridgeKind { childs: Vec::new() }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };

                self.create_bridge(intf.if_name).await?;

                self.connector.local.add_interface(&v_iface).await?;
                Ok(v_iface)
            }
            VirtualInterfaceConfigKind::VETH => {
                let external_face_name = self.generate_random_interface_name();
                let internal_iface_uuid = Uuid::new_v4();
                let external_iface_uuid = Uuid::new_v4();
                let v_iface_internal = VirtualInterface {
                    uuid: internal_iface_uuid,
                    if_name: intf.if_name.clone(),
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::VETH(VETHKind {
                        pair: external_iface_uuid,
                        internal: true,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                let v_iface_external = VirtualInterface {
                    uuid: external_iface_uuid,
                    if_name: external_face_name.clone(),
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::VETH(VETHKind {
                        pair: internal_iface_uuid,
                        internal: false,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };

                self.create_veth(intf.if_name, external_face_name).await?;

                self.connector
                    .local
                    .add_interface(&v_iface_internal)
                    .await?;
                self.connector
                    .local
                    .add_interface(&v_iface_external)
                    .await?;
                Ok(v_iface_internal)
            }
            VirtualInterfaceConfigKind::VLAN(conf) => {
                let ext_face = self.get_dataplane_from_config().await?;
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name.clone(),
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::VLAN(VLANKind {
                        tag: conf.tag,
                        dev: ext_face.clone(),
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };

                self.create_vlan(intf.if_name, ext_face.if_name, conf.tag)
                    .await?;

                self.connector.local.add_interface(&v_iface).await?;
                Ok(v_iface)
            }
            VirtualInterfaceConfigKind::MACVLAN => {
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name,
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::MACVLAN(MACVLANKind {
                        dev: self.get_dataplane_from_config().await?,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                Err(FError::Unimplemented)
                // self.connector
                //.local
                //.add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
            VirtualInterfaceConfigKind::GRE(conf) => {
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name,
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::GRE(GREKind {
                        local_addr: conf.local_addr,
                        remote_addr: conf.remote_addr,
                        ttl: conf.ttl,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                Err(FError::Unimplemented)
                // self.connector
                //.local
                //.add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
            VirtualInterfaceConfigKind::GRETAP(conf) => {
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name,
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::GRETAP(GREKind {
                        local_addr: conf.local_addr,
                        remote_addr: conf.remote_addr,
                        ttl: conf.ttl,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                Err(FError::Unimplemented)
                // self.connector
                //.local
                //.add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
            VirtualInterfaceConfigKind::IP6GRE(conf) => {
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name,
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::IP6GRE(GREKind {
                        local_addr: conf.local_addr,
                        remote_addr: conf.remote_addr,
                        ttl: conf.ttl,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                Err(FError::Unimplemented)
                // self.connector
                //.local
                //.add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
            VirtualInterfaceConfigKind::IP6GRETAP(conf) => {
                let v_iface = VirtualInterface {
                    uuid: Uuid::new_v4(),
                    if_name: intf.if_name,
                    net_ns: None,
                    parent: None,
                    kind: VirtualInterfaceKind::IP6GRETAP(GREKind {
                        local_addr: conf.local_addr,
                        remote_addr: conf.remote_addr,
                        ttl: conf.ttl,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                Err(FError::Unimplemented)
                // self.connector
                //.local
                //.add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
        }
    }

    async fn get_virtual_interface(&self, intf_uuid: Uuid) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        self.connector.local.get_interface(intf_uuid).await
    }

    async fn delete_virtual_interface(&self, intf_uuid: Uuid) -> FResult<VirtualInterface> {
        log::trace!("delete_virtual_interface({})", intf_uuid);
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match self.connector.local.get_interface(intf_uuid).await {
            Err(e) => {
                log::error!("Unable to find interface {}, error: {}", intf_uuid, e);
                Err(FError::NotFound)
            }
            Ok(intf) => {
                log::error!("Delete Interface: {:?}", intf);
                match intf.net_ns {
                    Some(ns_uuid) => {
                        let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                        let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                        let res = ns_manager.del_virtual_interface(intf.if_name.clone()).await;
                        log::info!(
                            "Result of del_virtual_interface({}) -> {:?}",
                            intf.if_name.clone(),
                            res
                        );
                        if let Err(e) = res? {
                            log::warn!(
                                "Got error {} from namespace manager when removing {}",
                                e,
                                intf.if_name
                            );
                            if let VirtualInterfaceKind::VETH(VETHKind { pair, internal }) =
                                intf.kind
                            {
                                if let Err(e) = self.connector.local.get_interface(pair).await {
                                    log::warn!("Other end of veth pair was already removed: {}", e);
                                    return Ok(intf);
                                }
                                return Err(FError::NetworkingError(
                                    "Veth peer not removed but interface not found in namespace"
                                        .to_string(),
                                ));
                            }
                            return Err(e);
                        }
                        self.connector.local.remove_interface(intf_uuid).await?;
                        Ok(intf)
                    }
                    None => {
                        if let VirtualInterfaceKind::VETH(ref info) = intf.kind {
                            if let Ok(pair) = self.connector.local.get_interface(info.pair).await {
                                self.del_iface(intf.if_name.clone()).await;
                                self.del_iface(pair.if_name.clone()).await;
                                self.connector.local.remove_interface(info.pair).await?;
                            } else {
                                log::trace!("Peer was alredy removed...");
                                self.del_iface(intf.if_name.clone()).await;
                            }
                        } else {
                            self.del_iface(intf.if_name.clone()).await?;
                        }
                        self.connector.local.remove_interface(intf_uuid).await?;
                        Ok(intf)
                    }
                }
            }
        }
    }

    async fn create_virtual_bridge(&self, br_name: String) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let v_iface = VirtualInterface {
            uuid: Uuid::new_v4(),
            if_name: br_name,
            net_ns: None,
            parent: None,
            kind: VirtualInterfaceKind::BRIDGE(BridgeKind { childs: Vec::new() }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        self.create_bridge(v_iface.if_name.clone()).await?;

        self.connector.local.add_interface(&v_iface).await?;
        Ok(v_iface)
    }

    async fn get_virtual_bridge(&self, br_uuid: Uuid) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match self.connector.local.get_interface(br_uuid).await {
            Err(err) => Err(err),
            Ok(i) => match i.kind {
                VirtualInterfaceKind::BRIDGE(_) => Ok(i),
                _ => Err(FError::WrongKind),
            },
        }
    }

    async fn delete_virtual_bridge(&self, br_uuid: Uuid) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match self.connector.local.get_interface(br_uuid).await {
            Err(err) => Err(err),
            Ok(i) => match i.net_ns {
                Some(ns_uuid) => {
                    let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                    let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                    ns_manager
                        .del_virtual_interface(i.if_name.clone())
                        .await??;
                    self.connector.local.remove_interface(br_uuid).await?;
                    Ok(i)
                }
                None => match i.kind {
                    VirtualInterfaceKind::BRIDGE(_) => {
                        self.del_iface(i.if_name.clone()).await?;
                        self.connector.local.remove_interface(br_uuid).await?;
                        Ok(i)
                    }
                    _ => Err(FError::WrongKind),
                },
            },
        }
    }

    async fn set_default_route_in_network_namespace(
        &self,
        ns_uuid: Uuid,
        intf_uuid: Uuid,
    ) -> FResult<()> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut netns = self.connector.local.get_network_namespace(ns_uuid).await?;
        let iface = self.connector.local.get_interface(intf_uuid).await?;
        match iface.net_ns {
            None => Err(FError::NotConnected),
            Some(nid) => {
                if nid == netns.uuid {
                    let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                    return ns_manager.set_default_route(iface.if_name.clone()).await?;
                }
                Err(FError::NotConnected)
            }
        }
    }

    async fn create_network_namespace(&self) -> FResult<NetworkNamespace> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let ns_name = self.generate_random_netns_name();
        let netns = NetworkNamespace {
            uuid: Uuid::new_v4(),
            ns_name: ns_name.clone(),
            interfaces: Vec::new(),
        };
        self.add_netns(ns_name.clone()).await?;

        self.spawn_ns_manager(ns_name.clone(), netns.uuid).await?;
        let ns_manager = self.get_ns_manager(&netns.uuid).await?;

        while !ns_manager.verify_server().await? {
            task::sleep(Duration::from_micros((100))).await;
        }

        ns_manager
            .set_virtual_interface_up("lo".to_string())
            .await??;

        self.connector.local.add_network_namespace(&netns).await?;
        Ok(netns)
    }

    async fn get_network_namespace(&self, ns_uuid: Uuid) -> FResult<NetworkNamespace> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        self.connector.local.get_network_namespace(ns_uuid).await
    }

    async fn delete_network_namespace(&self, ns_uuid: Uuid) -> FResult<NetworkNamespace> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match self.connector.local.get_network_namespace(ns_uuid).await {
            Err(_) => Err(FError::NotFound),
            Ok(netns) => {
                self.del_netns(netns.ns_name.clone()).await?;
                log::trace!("Taking guard to remove ns-manager");
                self.kill_ns_manager(&netns.uuid).await?;
                self.connector
                    .local
                    .remove_network_namespace(ns_uuid)
                    .await?;
                Ok(netns)
            }
        }
    }

    async fn bind_interface_to_connection_point(
        &self,
        intf_uuid: Uuid,
        cp_uuid: Uuid,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let cp = self.connector.local.get_connection_point(cp_uuid).await?;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;

        Err(FError::Unimplemented)
        // iface.net_ns = Some(cp.net_ns);
        // self.connector
        //     .local
        //     .add_interface(&iface)
        //     .await?;
        // Ok(iface)
    }

    async fn unbind_interface_from_connection_point(
        &self,
        intf_uuid: Uuid,
        cp_uuid: Uuid,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let cp = self.connector.local.get_connection_point(cp_uuid).await?;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;

        Err(FError::Unimplemented)

        // match iface.net_ns {
        //     Some(ns) => {
        //         if ns == cp.net_ns {
        //             iface.net_ns = None;
        //             self.connector
        //                 .loccal
        //                 .add_interface(&iface)
        //                 .await?;
        //             return Ok(iface);
        //         }
        //         Err(FError::NotConnected)
        //     }
        //     None => Err(FError::NotConnected),
        // }
    }

    async fn bind_connection_point_to_virtual_network(
        &self,
        cp_uuid: Uuid,
        vnet_uuid: Uuid,
    ) -> FResult<ConnectionPoint> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let cp = self.connector.local.get_connection_point(cp_uuid).await?;
        let mut vnet = self.connector.local.get_virtual_network(vnet_uuid).await?;
        Err(FError::Unimplemented)
        // vnet.connection_points.push(cp.uuid);
        // self.connector
        //     .local
        //     .add_virutal_network(&vnet)
        //     .await?;
        // Ok(cp)
    }

    async fn unbind_connection_point_from_virtual_network(
        &self,
        cp_uuid: Uuid,
        vnet_uuid: Uuid,
    ) -> FResult<ConnectionPoint> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let cp = self.connector.local.get_connection_point(cp_uuid).await?;
        let mut vnet = self.connector.local.get_virtual_network(vnet_uuid).await?;
        Err(FError::Unimplemented)
        // match vnet.connection_points.iter().position(|&x| x == cp.uuid) {
        //     Some(p) => {
        //         vnet.connection_points.remove(p);
        //         self.connector
        //             .local
        //             .add_virutal_network(&vnet)
        //             .await?;
        //         Ok(cp)
        //     }
        //     None => Err(FError::NotConnected),
        // }
    }

    async fn get_interface_addresses(&self, intf_uuid: Uuid) -> FResult<Vec<IPAddress>> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let iface = self.connector.local.get_interface(intf_uuid).await?;
        Ok(iface.addresses)
    }

    async fn get_overlay_iface(&self) -> FResult<String> {
        Ok(self.get_overlay_face_from_config().await?.if_name)
    }
    async fn get_vlan_face(&self) -> FResult<String> {
        Ok(self.get_dataplane_from_config().await?.if_name)
    }

    async fn create_macvlan_interface(&self, master_intf: String) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let v_iface = VirtualInterface {
            uuid: Uuid::new_v4(),
            if_name: self.generate_random_interface_name(),
            net_ns: None,
            parent: None,
            kind: VirtualInterfaceKind::MACVLAN(MACVLANKind {
                dev: Interface {
                    if_name: master_intf,
                    kind: InterfaceKind::ETHERNET,
                    addresses: Vec::new(),
                    phy_address: None,
                },
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };
        Err(FError::Unimplemented)
        // self.connector
        //     .local
        //     .add_interface(&v_iface)
        //     .await?;
        // Ok(v_iface)
    }

    async fn delete_macvan_interface(&self, intf_uuid: Uuid) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        match self.connector.local.get_interface(intf_uuid).await {
            Err(err) => Err(err),
            Ok(i) => match i.net_ns {
                Some(ns_uuid) => {
                    let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                    let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                    ns_manager
                        .del_virtual_interface(i.if_name.clone())
                        .await??;
                    self.connector.local.remove_interface(intf_uuid).await?;
                    Ok(i)
                }
                None => match i.kind {
                    VirtualInterfaceKind::MACVLAN(_) => {
                        self.del_iface(i.if_name.clone()).await?;
                        self.connector.local.remove_interface(intf_uuid).await?;
                        Ok(i)
                    }
                    _ => Err(FError::WrongKind),
                },
            },
        }
    }

    async fn move_interface_info_namespace(
        &self,
        intf_uuid: Uuid,
        ns_uuid: Uuid,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;

        match iface.net_ns {
            Some(old_ns_uuid) => {
                let mut netns = self
                    .connector
                    .local
                    .get_network_namespace(old_ns_uuid)
                    .await?;
                let mut newns = self.connector.local.get_network_namespace(ns_uuid).await?;

                match netns.interfaces.iter().position(|&x| x == intf_uuid) {
                    Some(p) => {
                        let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                        ns_manager
                            .move_virtual_interface_into_default_ns(iface.if_name.clone())
                            .await??;
                        netns.interfaces.remove(p);

                        self.set_iface_ns(iface.if_name.clone(), newns.ns_name.clone())
                            .await?;

                        iface.net_ns = Some(newns.uuid);
                        newns.interfaces.push(iface.uuid);

                        self.connector.local.add_interface(&iface).await?;
                        self.connector.local.add_network_namespace(&netns).await?;
                        Ok(iface)
                    }
                    None => Err(FError::NotConnected),
                }
            }
            None => {
                let mut netns = self.connector.local.get_network_namespace(ns_uuid).await?;

                self.set_iface_ns(iface.if_name.clone(), netns.ns_name.clone())
                    .await?;

                iface.net_ns = Some(netns.uuid);
                netns.interfaces.push(iface.uuid);

                self.connector.local.add_interface(&iface).await?;
                self.connector.local.add_network_namespace(&netns).await?;
                Ok(iface)
            }
        }
    }

    async fn move_interface_into_default_namespace(
        &self,
        intf_uuid: Uuid,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;
        match iface.net_ns {
            Some(netns_uuid) => {
                let mut netns = self
                    .connector
                    .local
                    .get_network_namespace(netns_uuid)
                    .await?;
                let ns_manager = self.get_ns_manager(&netns_uuid).await?;
                ns_manager
                    .move_virtual_interface_into_default_ns(iface.if_name.clone())
                    .await??;
                iface.net_ns = None;
                self.connector.local.add_interface(&iface).await?;
                match netns.interfaces.iter().position(|&x| x == iface.uuid) {
                    Some(p) => {
                        netns.interfaces.remove(p);
                        self.connector.local.add_network_namespace(&netns).await?;
                        Ok(iface)
                    }
                    None => Err(FError::NotConnected),
                }
            }
            None => Ok(iface),
        }
    }

    async fn rename_virtual_interface(
        &self,
        intf_uuid: Uuid,
        intf_name: String,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;
        match iface.net_ns {
            Some(ns_uuid) => {
                let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                ns_manager
                    .set_virtual_interface_name(iface.if_name.clone(), intf_name.clone())
                    .await??;
                iface.if_name = intf_name;
                self.connector.local.add_interface(&iface).await?;
                Ok(iface)
            }
            None => {
                self.set_iface_name(iface.if_name.clone(), intf_name.clone())
                    .await?;
                iface.if_name = intf_name;
                self.connector.local.add_interface(&iface).await?;
                Ok(iface)
            }
        }
    }

    async fn attach_interface_to_bridge(
        &self,
        intf_uuid: Uuid,
        br_uuid: Uuid,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;
        let bridge = self.connector.local.get_interface(br_uuid).await?;
        match bridge.kind {
            VirtualInterfaceKind::BRIDGE(mut info) => match (iface.net_ns, bridge.net_ns) {
                (Some(ns_uuid), Some(_)) => {
                    let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                    let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                    ns_manager
                        .set_virtual_interface_master(iface.if_name.clone(), bridge.if_name.clone())
                        .await??;

                    iface.parent = Some(bridge.uuid);
                    info.childs.push(iface.uuid);

                    ns_manager
                        .set_virtual_interface_up(iface.if_name.clone())
                        .await??;

                    let mut new_bridge = self.connector.local.get_interface(br_uuid).await?;
                    new_bridge.kind = VirtualInterfaceKind::BRIDGE(info);
                    self.connector.local.add_interface(&iface).await?;
                    self.connector.local.add_interface(&new_bridge).await?;
                    Ok(iface)
                }
                (Some(_), None) | (None, Some(_)) => Err(FError::NetworkingError(String::from(
                    "Interface in different namespaces",
                ))),
                (None, None) => {
                    self.set_iface_master(iface.if_name.clone(), bridge.if_name.clone())
                        .await?;

                    iface.parent = Some(bridge.uuid);
                    info.childs.push(iface.uuid);

                    self.set_iface_up(iface.if_name.clone()).await?;

                    let mut new_bridge = self.connector.local.get_interface(br_uuid).await?;
                    new_bridge.kind = VirtualInterfaceKind::BRIDGE(info);
                    self.connector.local.add_interface(&iface).await?;
                    self.connector.local.add_interface(&new_bridge).await?;
                    Ok(iface)
                }
            },
            _ => Err(FError::WrongKind),
        }
    }

    async fn detach_interface_from_bridge(&self, intf_uuid: Uuid) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;
        match iface.parent {
            None => Err(FError::NotConnected),
            Some(br_uuid) => {
                let bridge = self.connector.local.get_interface(br_uuid).await?;
                match bridge.kind {
                    VirtualInterfaceKind::BRIDGE(mut info) => match iface.net_ns {
                        Some(ns_uuid) => {
                            let ns_manager = self.get_ns_manager(&ns_uuid).await?;

                            iface.parent = None;

                            match info.childs.iter().position(|&x| x == iface.uuid) {
                                Some(p) => {
                                    info.childs.remove(p);
                                    let mut new_bridge =
                                        self.connector.local.get_interface(br_uuid).await?;
                                    ns_manager
                                        .set_virtual_interface_nomaster(iface.if_name.clone())
                                        .await??;
                                    new_bridge.kind = VirtualInterfaceKind::BRIDGE(info);
                                    self.connector.local.add_interface(&new_bridge).await?;
                                    self.connector.local.add_interface(&iface).await?;
                                    return Ok(iface);
                                }
                                None => return Err(FError::NotConnected),
                            }
                        }
                        None => match info.childs.iter().position(|&x| x == iface.uuid) {
                            Some(p) => {
                                info.childs.remove(p);
                                let mut new_bridge =
                                    self.connector.local.get_interface(br_uuid).await?;
                                self.del_iface_master(iface.if_name.clone()).await?;
                                new_bridge.kind = VirtualInterfaceKind::BRIDGE(info);
                                self.connector.local.add_interface(&new_bridge).await?;
                                self.connector.local.add_interface(&iface).await?;
                                return Ok(iface);
                            }
                            None => return Err(FError::NotConnected),
                        },
                    },
                    _ => Err(FError::WrongKind),
                }
            }
        }

        // match bridge.kind {
        //     VirtualInterfaceKind::BRIDGE(mut info) => match iface.parent {
        //         Some(br) => {
        //             if br == bridge.uuid {
        //                 iface.parent = None;
        //                 self.connector
        //                     .global
        //                     .add_node_interface(node_uuid, &iface)
        //                     .await?;
        //                 match info.childs.iter().position(|&x| x == iface.uuid) {
        //                     Some(p) => {
        //                         info.childs.remove(p);
        //                         let mut new_bridge = self
        //                             .connector
        //                             .global
        //                             .get_node_interface(node_uuid, br_uuid)
        //                             .await?;
        //                         self.del_iface_master(iface.if_name.clone()).await?;
        //                         new_bridge.kind = VirtualInterfaceKind::BRIDGE(info);
        //                         self.connector
        //                             .global
        //                             .add_node_interface(node_uuid, &new_bridge)
        //                             .await?;
        //                         return Ok(iface);
        //                     }
        //                     None => return Err(FError::NotConnected),
        //                 }
        //             }
        //             Err(FError::NotConnected)
        //         }
        //         None => Err(FError::NotConnected),
        //     },
        //     _ => Err(FError::WrongKind),
        // }
    }

    async fn create_virtual_interface_in_namespace(
        &self,
        intf: VirtualInterfaceConfig,
        ns_uuid: Uuid,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut netns = self.connector.local.get_network_namespace(ns_uuid).await?;
        //Err(FError::Unimplemented)
        match intf.kind {
            VirtualInterfaceConfigKind::VXLAN(conf) => {
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::VXLAN(VXLANKind {
                //         vni: conf.vni,
                //         mcast_addr: conf.mcast_addr,
                //         port: conf.port,
                //         dev: self.get_overlay_face_from_config().await?,
                //     }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
                Err(FError::Unimplemented)
            }
            VirtualInterfaceConfigKind::BRIDGE => {
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::BRIDGE(BridgeKind { childs: Vec::new() }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
                Err(FError::Unimplemented)
            }
            VirtualInterfaceConfigKind::VETH => {
                let external_face_name = self.generate_random_interface_name();
                let internal_iface_uuid = Uuid::new_v4();
                let external_iface_uuid = Uuid::new_v4();
                let v_iface_internal = VirtualInterface {
                    uuid: internal_iface_uuid,
                    if_name: intf.if_name,
                    net_ns: Some(netns.uuid),
                    parent: None,
                    kind: VirtualInterfaceKind::VETH(VETHKind {
                        pair: external_iface_uuid,
                        internal: true,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                let v_iface_external = VirtualInterface {
                    uuid: external_iface_uuid,
                    if_name: external_face_name.clone(),
                    net_ns: Some(netns.uuid),
                    parent: None,
                    kind: VirtualInterfaceKind::VETH(VETHKind {
                        pair: internal_iface_uuid,
                        internal: false,
                    }),
                    addresses: Vec::new(),
                    phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                };
                let ns_manager = self.get_ns_manager(&ns_uuid).await?;

                ns_manager
                    .add_virtual_interface_veth(
                        v_iface_internal.if_name.clone(),
                        external_face_name.clone(),
                    )
                    .await??;

                netns.interfaces.push(internal_iface_uuid);
                netns.interfaces.push(external_iface_uuid);
                self.connector.local.add_network_namespace(&netns).await?;
                self.connector
                    .local
                    .add_interface(&v_iface_internal)
                    .await?;
                self.connector
                    .local
                    .add_interface(&v_iface_external)
                    .await?;
                Ok(v_iface_internal)
            }
            VirtualInterfaceConfigKind::VLAN(conf) => {
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::VLAN(VLANKind {
                //         tag: conf.tag,
                //         dev: self.get_dataplane_from_config().await?,
                //     }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
                Err(FError::Unimplemented)
            }
            VirtualInterfaceConfigKind::MACVLAN => {
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::MACVLAN(MACVLANKind {
                //         dev: self.get_dataplane_from_config().await?,
                //     }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
                Err(FError::Unimplemented)
            }
            VirtualInterfaceConfigKind::GRE(conf) => {
                Err(FError::Unimplemented)
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::GRE(GREKind {
                //         local_addr: conf.local_addr,
                //         remote_addr: conf.remote_addr,
                //         ttl: conf.ttl,
                //     }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
            VirtualInterfaceConfigKind::GRETAP(conf) => {
                Err(FError::Unimplemented)
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::GRETAP(GREKind {
                //         local_addr: conf.local_addr,
                //         remote_addr: conf.remote_addr,
                //         ttl: conf.ttl,
                //     }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
            VirtualInterfaceConfigKind::IP6GRE(conf) => {
                Err(FError::Unimplemented)
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::IP6GRE(GREKind {
                //         local_addr: conf.local_addr,
                //         remote_addr: conf.remote_addr,
                //         ttl: conf.ttl,
                //     }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
            VirtualInterfaceConfigKind::IP6GRETAP(conf) => {
                Err(FError::Unimplemented)
                // let v_iface = VirtualInterface {
                //     uuid: Uuid::new_v4(),
                //     if_name: intf.if_name,
                //     net_ns: Some(netns.uuid),
                //     parent: None,
                //     kind: VirtualInterfaceKind::IP6GRETAP(GREKind {
                //         local_addr: conf.local_addr,
                //         remote_addr: conf.remote_addr,
                //         ttl: conf.ttl,
                //     }),
                //     addresses: Vec::new(),
                //     phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
                // };
                // netns.interfaces.push(v_iface.uuid);
                // self.connector
                //     .local
                //     .add_network_namespace(&netns)
                //     .await?;
                // self.connector
                //     .local
                //     .add_interface(&v_iface)
                //     .await?;
                // Ok(v_iface)
            }
        }
    }

    async fn delete_virtual_interface_in_namespace(
        &self,
        intf_uuid: Uuid,
        ns_uuid: Uuid,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut netns = self.connector.local.get_network_namespace(ns_uuid).await?;
        let iface = self.connector.local.get_interface(intf_uuid).await?;
        match iface.net_ns {
            None => Err(FError::NotConnected),
            Some(nid) => {
                if nid == netns.uuid {
                    let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                    ns_manager
                        .del_virtual_interface(iface.if_name.clone())
                        .await??;

                    match netns.interfaces.iter().position(|&x| x == iface.uuid) {
                        Some(p) => {
                            netns.interfaces.remove(p);
                            if let VirtualInterfaceKind::VETH(ref info) = iface.kind {
                                self.connector.local.remove_interface(info.pair).await?;
                            }
                            self.connector.local.add_network_namespace(&netns).await?;
                            self.connector.local.remove_interface(intf_uuid).await?;
                            return Ok(iface);
                        }
                        None => return Err(FError::NotConnected),
                    }
                }
                Err(FError::NotConnected)
            }
        }
    }

    async fn assing_address_to_interface(
        &self,
        intf_uuid: Uuid,
        address: Option<IpNetwork>,
    ) -> FResult<VirtualInterface> {
        log::trace!("assing_address_to_interface {} {:?}", intf_uuid, address);
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;
        match iface.net_ns {
            Some(ns_uuid) => {
                let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                let addresses = ns_manager
                    .add_virtual_interface_address(iface.if_name.clone(), address)
                    .await??;
                iface.addresses = addresses;
                self.connector.local.add_interface(&iface).await?;
                Ok(iface)
            }
            None => match address {
                Some(address) => {
                    self.add_iface_address(iface.if_name.clone(), address.ip(), address.prefix())
                        .await?;
                    iface.addresses.push(address.ip());
                    self.connector.local.add_interface(&iface).await?;
                    Ok(iface)
                }
                None => {
                    // If the address is None we spawn a DHCP client
                    // and then we the the address from netlink
                    let mut child = Command::new("dhclient")
                        .arg("-i")
                        .arg(&iface.if_name.clone())
                        .spawn()
                        .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
                    child
                        .wait()
                        .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
                    let addresses = self.get_iface_addresses(iface.if_name.clone()).await?;
                    iface.addresses = addresses;
                    self.connector.local.add_interface(&iface).await?;
                    Ok(iface)
                }
            },
        }
    }

    async fn remove_address_from_interface(
        &self,
        intf_uuid: Uuid,
        address: IPAddress,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;
        match iface.net_ns {
            Some(ns_uuid) => match iface.addresses.iter().position(|&x| x == address) {
                Some(p) => {
                    let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                    let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                    let addresses = ns_manager
                        .del_virtual_interface_address(iface.if_name.clone(), address)
                        .await??;
                    iface.addresses.remove(p);
                    self.connector.local.add_interface(&iface).await?;
                    Ok(iface)
                }
                None => Err(FError::NotConnected),
            },
            None => match iface.addresses.iter().position(|&x| x == address) {
                Some(p) => {
                    self.del_iface_address(iface.if_name.clone(), address)
                        .await?;
                    iface.addresses.remove(p);
                    self.connector.local.add_interface(&iface).await?;
                    Ok(iface)
                }
                None => Err(FError::NotConnected),
            },
        }
    }

    async fn set_macaddres_of_interface(
        &self,
        intf_uuid: Uuid,
        address: MACAddress,
    ) -> FResult<VirtualInterface> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let mut iface = self.connector.local.get_interface(intf_uuid).await?;

        let vec_addr = vec![
            address.0, address.1, address.2, address.3, address.4, address.5,
        ];
        match iface.net_ns {
            Some(ns_uuid) => {
                let netns = self.connector.local.get_network_namespace(ns_uuid).await?;
                let ns_manager = self.get_ns_manager(&ns_uuid).await?;
                ns_manager
                    .set_virtual_interface_mac(iface.if_name.clone(), vec_addr)
                    .await??;
                iface.phy_address = address;
                self.connector.local.add_interface(&iface).await?;
                Ok(iface)
            }
            None => {
                self.set_iface_mac(iface.if_name.clone(), vec_addr).await?;
                iface.phy_address = address;
                self.connector.local.add_interface(&iface).await?;
                Ok(iface)
            }
        }
    }
}

impl LinuxNetwork {
    pub async fn new(
        z: Arc<zenoh::net::Session>,
        connector: Arc<fog05_sdk::zconnector::ZConnector>,
        pid: u32,
        config: LinuxNetworkConfig,
    ) -> FResult<Self> {
        // this will be removed once netlink merges the async-std support
        let (connection, handle, _) = new_connection().unwrap();
        async_std::task::spawn(connection);

        let state = LinuxNetworkState {
            uuid: None,
            nl_handler: handle,
            ns_managers: HashMap::new(),
        };

        Ok(Self {
            z,
            connector,
            pid,
            agent: None,
            os: None,
            config,
            state: Arc::new(RwLock::new(state)),
        })
    }

    async fn run(&self, stop: async_std::channel::Receiver<()>) -> FResult<()> {
        info!("LinuxNetwork main loop starting...");

        //starting the Agent-Plugin Server
        let hv_server = self
            .clone()
            .get_networking_plugin_server(self.z.clone(), None);
        let (stopper, _h) = hv_server.connect().await?;
        hv_server.initialize().await?;

        let mut guard = self.state.write().await;
        guard.uuid = Some(hv_server.instance_uuid());
        drop(guard);

        hv_server.register().await?;

        let (shv, _hhv) = hv_server.start().await?;

        let monitoring = async {
            loop {
                info!("Monitoring loop started");
                task::sleep(Duration::from_secs(60)).await;
            }
        };

        self.agent
            .clone()
            .unwrap()
            .register_plugin(hv_server.instance_uuid(), PluginKind::NETWORKING)
            .await??;

        match monitoring.race(stop.recv()).await {
            Ok(_) => trace!("Monitoring ending correct"),
            Err(e) => trace!("Monitoring ending got error: {}", e),
        }

        self.agent
            .clone()
            .unwrap()
            .unregister_plugin(hv_server.instance_uuid())
            .await??;

        hv_server.stop(shv).await?;
        hv_server.unregister().await?;
        hv_server.disconnect(stopper).await?;

        info!("LinuxNetwork main loop exiting");
        Ok(())
    }

    pub async fn start(
        &mut self,
    ) -> (
        async_std::channel::Sender<()>,
        async_std::task::JoinHandle<FResult<()>>,
    ) {
        let local_os = OSClient::find_local_servers(self.z.clone()).await.unwrap();
        if local_os.is_empty() {
            error!("Unable to find a local OS interface");
            panic!("No OS Server");
        }

        let local_agent = AgentPluginInterfaceClient::find_local_servers(self.z.clone())
            .await
            .unwrap();
        if local_agent.is_empty() {
            error!("Unable to find a local Agent interface");
            panic!("No Agent Server");
        }

        let os = OSClient::new(self.z.clone(), local_os[0]);
        let agent = AgentPluginInterfaceClient::new(self.z.clone(), local_agent[0]);

        self.agent = Some(agent);
        self.os = Some(os);

        // Starting main loop in a task
        let (s, r) = async_std::channel::bounded::<()>(1);
        let plugin = self.clone();
        let h = async_std::task::spawn_blocking(move || {
            async_std::task::block_on(async { plugin.run(r).await })
        });
        (s, h)
    }

    pub async fn stop(&self, stop: async_std::channel::Sender<()>) -> FResult<()> {
        log::debug!("Linux Network Stopping");
        stop.send(()).await;

        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;
        let default_vnet = self
            .connector
            .local
            .get_virtual_network(Uuid::nil())
            .await?;

        for iface_uuid in default_vnet.interfaces {
            let iface = self.connector.local.get_interface(iface_uuid).await?;
            match iface.net_ns {
                None => {
                    self.del_iface(iface.if_name.clone()).await?;
                    self.connector.local.remove_interface(iface_uuid).await?;
                }
                Some(_) => continue,
            }
        }

        if let Some(internals) = default_vnet.plugin_internals {
            let internals = deserialize_network_internals(internals.as_slice())?;

            // Removing namespace if present
            if let Some(ns_internals) = internals.associated_netns {
                self.connector
                    .local
                    .get_network_namespace(ns_internals.ns_uuid)
                    .await?;

                self.del_netns(ns_internals.ns_name).await?;

                log::trace!("Taking guard to remove ns-manager");
                self.kill_ns_manager(&ns_internals.ns_uuid).await?;
                self.connector
                    .local
                    .remove_network_namespace(ns_internals.ns_uuid)
                    .await?;
            }

            // Killing dhcp if present
            if let Some(dhcp_internal) = internals.dhcp {
                let str_pid = String::from_utf8(
                    self.os
                        .as_ref()
                        .unwrap()
                        .read_file(dhcp_internal.pid_file.clone())
                        .await??,
                )
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
                let pid = str_pid
                    .trim()
                    .parse::<i32>()
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?;

                log::trace!("Killing dnsmasq {}", pid);

                kill(Pid::from_raw(pid), Signal::SIGKILL)
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?;

                async_std::fs::remove_file(async_std::path::Path::new(&dhcp_internal.pid_file))
                    .await?;
                async_std::fs::remove_file(async_std::path::Path::new(&dhcp_internal.leases_file))
                    .await?;
                async_std::fs::remove_file(async_std::path::Path::new(&dhcp_internal.conf)).await?;
                async_std::fs::remove_file(async_std::path::Path::new(&dhcp_internal.log_file))
                    .await?;
            }

            for table in internals.associated_tables {
                self.clean_nat(table).await?;
            }
        }

        self.connector
            .local
            .remove_virtual_network(Uuid::nil())
            .await?;

        // Here we should remove and kill all the others ns-managers and clean-up

        Ok(())
    }

    /// Spawns and insert a new Namespace Manager into the Plugin state
    async fn spawn_ns_manager(&self, ns_name: String, ns_uuid: Uuid) -> FResult<()> {
        let mut guard = self.state.write().await;
        let child = Command::new("fos-net-linux-ns-manager")
            .arg("--netns")
            .arg(&ns_name)
            .arg("--id")
            .arg(format!("{}", ns_uuid))
            .arg("--locator")
            .arg(self.config.zfilelocator.clone())
            .spawn()
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
        let ns_manager_client = NamespaceManagerClient::new(self.z.clone(), ns_uuid);
        guard
            .ns_managers
            .insert(ns_uuid, (child.id(), ns_manager_client));
        drop(guard);
        Ok(())
    }

    async fn get_ns_manager(&self, ns_uuid: &Uuid) -> FResult<NamespaceManagerClient> {
        let mut guard = self.state.read().await;
        let (_, ns_manager) = guard
            .ns_managers
            .get(ns_uuid)
            .ok_or_else(|| FError::NetworkingError("Manager not found".to_string()))?;
        Ok(ns_manager.clone())
    }

    async fn remove_ns_manager(&self, ns_uuid: &Uuid) -> FResult<(u32, NamespaceManagerClient)> {
        let mut guard = self.state.write().await;
        let (pid, ns_manager) = guard
            .ns_managers
            .remove(&ns_uuid)
            .ok_or_else(|| FError::NetworkingError("Manager not found".to_string()))?;
        Ok((pid, ns_manager))
    }

    /// Removes and kills a Namespaces Manager
    async fn kill_ns_manager(&self, ns_uuid: &Uuid) -> FResult<()> {
        let (pid, ns_manager) = self.remove_ns_manager(ns_uuid).await?;
        kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
        Ok(())
    }

    async fn mcast_vxlan_create(
        &self,
        mut vnet: VirtualNetwork,
        vxlan_info: MCastVXLANInfo,
    ) -> FResult<VirtualNetwork> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;

        // Generating Names

        let br_uuid = Uuid::new_v4();
        let br_name = self.generate_random_interface_name();

        let vxl_uuid = Uuid::new_v4();
        let vxl_name = self.generate_random_interface_name();

        let internal_br_uuid = Uuid::new_v4();
        let internal_br_name = self.generate_random_interface_name();

        let internal_veth_uuid = Uuid::new_v4();
        let internal_veth_name = self.generate_random_interface_name();

        let external_veth_uuid = Uuid::new_v4();
        let external_veth_name = self.generate_random_interface_name();

        let mut associated_ns = NetworkNamespace {
            uuid: vnet.uuid,
            ns_name: self.generate_random_netns_name(),
            interfaces: vec![
                external_veth_uuid,
                internal_veth_uuid,
                internal_br_uuid,
                vxl_uuid,
                br_uuid,
            ],
        };

        // Generating Structs

        let v_bridge = VirtualInterface {
            uuid: br_uuid,
            if_name: br_name.clone(),
            net_ns: None,
            parent: None,
            kind: VirtualInterfaceKind::BRIDGE(BridgeKind {
                childs: vec![external_veth_uuid, vxl_uuid],
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let v_internal_bridge = VirtualInterface {
            uuid: internal_br_uuid,
            if_name: internal_br_name.clone(),
            net_ns: Some(associated_ns.uuid),
            parent: None,
            kind: VirtualInterfaceKind::BRIDGE(BridgeKind {
                childs: vec![internal_veth_uuid],
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let vxl_iface = VirtualInterface {
            uuid: vxl_uuid,
            if_name: vxl_name.clone(),
            net_ns: None,
            parent: Some(br_uuid),
            kind: VirtualInterfaceKind::VXLAN(VXLANKind {
                vni: vxlan_info.vni,
                port: vxlan_info.port,
                mcast_addr: vxlan_info.mcast_addr,
                dev: Interface {
                    if_name: self.get_overlay_iface().await?,
                    kind: InterfaceKind::ETHERNET,
                    addresses: Vec::new(),
                    phy_address: None,
                },
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let v_veth_i = VirtualInterface {
            uuid: internal_veth_uuid,
            if_name: internal_veth_name.clone(),
            net_ns: Some(associated_ns.uuid),
            parent: Some(internal_br_uuid),
            kind: VirtualInterfaceKind::VETH(VETHKind {
                pair: external_veth_uuid,
                internal: true,
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let v_veth_e = VirtualInterface {
            uuid: external_veth_uuid,
            if_name: external_veth_name.clone(),
            net_ns: None,
            parent: Some(br_uuid),
            kind: VirtualInterfaceKind::VETH(VETHKind {
                pair: internal_veth_uuid,
                internal: false,
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        // Creating Virtual network bridge

        self.create_bridge(br_name.clone()).await?;
        self.connector.local.add_interface(&v_bridge).await?;

        vnet.interfaces.push(br_uuid);

        self.set_iface_up(br_name.clone()).await?;

        // Creating VXLAN Interface

        self.create_mcast_vxlan(
            vxl_name.clone(),
            self.get_overlay_iface().await?,
            vxlan_info.vni,
            vxlan_info.mcast_addr,
            vxlan_info.port,
        )
        .await?;
        self.connector.local.add_interface(&vxl_iface).await?;

        vnet.interfaces.push(vxl_uuid);

        self.set_iface_master(vxl_name.clone(), br_name.clone())
            .await?;
        self.set_iface_up(vxl_name).await?;

        // Creating netns and spawing the namespace manager
        self.add_netns(associated_ns.ns_name.clone()).await?;
        self.spawn_ns_manager(associated_ns.ns_name.clone(), associated_ns.uuid)
            .await?;

        self.connector
            .local
            .add_network_namespace(&associated_ns)
            .await?;

        // Creating veth pair
        self.create_veth(external_veth_name.clone(), internal_veth_name.clone())
            .await?;

        self.connector.local.add_interface(&v_veth_e).await?;

        vnet.interfaces.push(internal_veth_uuid);

        self.connector.local.add_interface(&v_veth_i).await?;

        vnet.interfaces.push(external_veth_uuid);

        self.set_iface_master(external_veth_name.clone(), br_name.clone())
            .await?;
        self.set_iface_up(external_veth_name).await?;

        self.set_iface_ns(
            internal_veth_name.clone(),
            associated_ns.ns_name.clone().clone(),
        )
        .await?;

        // create internal bridge
        let ns_manager = self.get_ns_manager(&associated_ns.uuid).await?;

        // This is used to wait that the namespace manager is ready to serve
        while !ns_manager.verify_server().await? {}

        ns_manager
            .set_virtual_interface_up("lo".to_string())
            .await??;

        ns_manager
            .add_virtual_interface_bridge(internal_br_name.clone())
            .await??;

        ns_manager
            .set_virtual_interface_up(internal_br_name.clone())
            .await??;

        vnet.interfaces.push(internal_br_uuid);

        self.connector
            .local
            .add_interface(&v_internal_bridge)
            .await?;

        ns_manager
            .set_virtual_interface_master(internal_veth_name.clone(), internal_br_name.clone())
            .await??;

        ns_manager
            .set_virtual_interface_up(internal_veth_name.clone())
            .await??;

        // NAT configuration, skip it for the time being...
        // let nat_table = self
        //     .configure_nat(
        //         IpNetwork::V4(
        //             ipnetwork::Ipv4Network::new(
        //                 std::net::Ipv4Addr::new(10, 240, 0, 0),
        //                 16,
        //             )
        //             .map_err(|e| FError::NetworkingError(format!("{}", e)))?,
        //         ),
        //         &self.get_overlay_face_from_config().await?.if_name,
        //     )
        //     .await?;

        // DHCP configuration and spawn

        let dhcp_internal = match &vnet.ip_configuration {
            Some(conf) => None,
            None => None,
        };

        let ns_info = Some(VNetNetns {
            ns_name: associated_ns.ns_name.clone(),
            ns_uuid: associated_ns.uuid,
        });

        let internals = VirtualNetworkInternals {
            associated_netns: ns_info,
            dhcp: dhcp_internal,
            associated_tables: vec![],
        };
        vnet.plugin_internals = Some(serialize_network_internals(&internals)?);
        Ok(vnet)
    }

    async fn ptp_vxlan_create(
        &self,
        mut vnet: VirtualNetwork,
        vxlan_info: P2PVXLANInfo,
    ) -> FResult<VirtualNetwork> {
        let node_uuid = self.agent.as_ref().unwrap().get_node_uuid().await??;

        // Generating Names

        let br_uuid = Uuid::new_v4();
        let br_name = self.generate_random_interface_name();

        let vxl_uuid = Uuid::new_v4();
        let vxl_name = self.generate_random_interface_name();

        let internal_br_uuid = Uuid::new_v4();
        let internal_br_name = self.generate_random_interface_name();

        let internal_veth_uuid = Uuid::new_v4();
        let internal_veth_name = self.generate_random_interface_name();

        let external_veth_uuid = Uuid::new_v4();
        let external_veth_name = self.generate_random_interface_name();

        let mut associated_ns = NetworkNamespace {
            uuid: vnet.uuid,
            ns_name: self.generate_random_netns_name(),
            interfaces: vec![
                external_veth_uuid,
                internal_veth_uuid,
                internal_br_uuid,
                vxl_uuid,
                br_uuid,
            ],
        };

        // Generating Structs

        let v_bridge = VirtualInterface {
            uuid: br_uuid,
            if_name: br_name.clone(),
            net_ns: None,
            parent: None,
            kind: VirtualInterfaceKind::BRIDGE(BridgeKind {
                childs: vec![external_veth_uuid, vxl_uuid],
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let v_internal_bridge = VirtualInterface {
            uuid: internal_br_uuid,
            if_name: internal_br_name.clone(),
            net_ns: Some(associated_ns.uuid),
            parent: None,
            kind: VirtualInterfaceKind::BRIDGE(BridgeKind {
                childs: vec![internal_veth_uuid],
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let vxl_iface = VirtualInterface {
            uuid: vxl_uuid,
            if_name: vxl_name.clone(),
            net_ns: None,
            parent: Some(br_uuid),
            kind: VirtualInterfaceKind::VXLAN(VXLANKind {
                vni: vxlan_info.vni,
                port: vxlan_info.port,
                mcast_addr: vxlan_info.remote_addr,
                dev: Interface {
                    if_name: self.get_overlay_iface().await?,
                    kind: InterfaceKind::ETHERNET,
                    addresses: Vec::new(),
                    phy_address: None,
                },
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let v_veth_i = VirtualInterface {
            uuid: internal_veth_uuid,
            if_name: internal_veth_name.clone(),
            net_ns: Some(associated_ns.uuid),
            parent: Some(internal_br_uuid),
            kind: VirtualInterfaceKind::VETH(VETHKind {
                pair: external_veth_uuid,
                internal: true,
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        let v_veth_e = VirtualInterface {
            uuid: external_veth_uuid,
            if_name: external_veth_name.clone(),
            net_ns: None,
            parent: Some(br_uuid),
            kind: VirtualInterfaceKind::VETH(VETHKind {
                pair: internal_veth_uuid,
                internal: false,
            }),
            addresses: Vec::new(),
            phy_address: MACAddress::new(0, 0, 0, 0, 0, 0),
        };

        // Creating Virtual network bridge

        self.create_bridge(br_name.clone()).await?;
        self.connector.local.add_interface(&v_bridge).await?;

        vnet.interfaces.push(br_uuid);

        self.set_iface_up(br_name.clone()).await?;

        // Creating VXLAN Interface

        let overlay_iface_address = *self
            .get_overlay_face_from_config()
            .await?
            .addresses
            .first()
            .ok_or(FError::NotFound)?;
        self.create_ptp_vxlan(
            vxl_name.clone(),
            self.get_overlay_iface().await?,
            vxlan_info.vni,
            overlay_iface_address,
            vxlan_info.remote_addr,
            vxlan_info.port,
        )
        .await?;
        self.connector.local.add_interface(&vxl_iface).await?;

        vnet.interfaces.push(vxl_uuid);

        self.set_iface_master(vxl_name.clone(), br_name.clone())
            .await?;
        self.set_iface_up(vxl_name).await?;

        // Creating netns and spawing the namespace manager
        self.add_netns(associated_ns.ns_name.clone()).await?;
        self.spawn_ns_manager(associated_ns.ns_name.clone(), associated_ns.uuid)
            .await?;

        self.connector
            .local
            .add_network_namespace(&associated_ns)
            .await?;

        // Creating veth pair
        self.create_veth(external_veth_name.clone(), internal_veth_name.clone())
            .await?;

        self.connector.local.add_interface(&v_veth_e).await?;

        vnet.interfaces.push(internal_veth_uuid);

        self.connector.local.add_interface(&v_veth_i).await?;

        vnet.interfaces.push(external_veth_uuid);

        self.set_iface_master(external_veth_name.clone(), br_name.clone())
            .await?;
        self.set_iface_up(external_veth_name).await?;

        self.set_iface_ns(
            internal_veth_name.clone(),
            associated_ns.ns_name.clone().clone(),
        )
        .await?;

        // create internal bridge
        let ns_manager = self.get_ns_manager(&associated_ns.uuid).await?;

        // This is used to wait that the namespace manager is ready to serve
        while !ns_manager.verify_server().await? {}

        ns_manager
            .set_virtual_interface_up("lo".to_string())
            .await??;

        ns_manager
            .add_virtual_interface_bridge(internal_br_name.clone())
            .await??;

        ns_manager
            .set_virtual_interface_up(internal_br_name.clone())
            .await??;

        vnet.interfaces.push(internal_br_uuid);

        self.connector
            .local
            .add_interface(&v_internal_bridge)
            .await?;

        ns_manager
            .set_virtual_interface_master(internal_veth_name.clone(), internal_br_name.clone())
            .await??;

        ns_manager
            .set_virtual_interface_up(internal_veth_name.clone())
            .await??;

        // NAT configuration, skip it for the time being...
        // let nat_table = self
        //     .configure_nat(
        //         IpNetwork::V4(
        //             ipnetwork::Ipv4Network::new(
        //                 std::net::Ipv4Addr::new(10, 240, 0, 0),
        //                 16,
        //             )
        //             .map_err(|e| FError::NetworkingError(format!("{}", e)))?,
        //         ),
        //         &self.get_overlay_face_from_config().await?.if_name,
        //     )
        //     .await?;

        // DHCP configuration and spawn

        let dhcp_internal = match &vnet.ip_configuration {
            Some(conf) => None,
            None => None,
        };

        let ns_info = Some(VNetNetns {
            ns_name: associated_ns.ns_name.clone(),
            ns_uuid: associated_ns.uuid,
        });

        let internals = VirtualNetworkInternals {
            associated_netns: ns_info,
            dhcp: dhcp_internal,
            associated_tables: vec![],
        };
        vnet.plugin_internals = Some(serialize_network_internals(&internals)?);
        Ok(vnet)
    }

    async fn get_overlay_face_from_config(&self) -> FResult<Interface> {
        let iface = self.config.overlay_iface.as_ref().ok_or(FError::NotFound)?;
        let addresses = self.get_iface_addresses(iface.clone()).await?;
        Ok(Interface {
            if_name: iface.to_string(),
            kind: InterfaceKind::ETHERNET,
            addresses,
            phy_address: None,
        })
    }

    async fn get_dataplane_from_config(&self) -> FResult<Interface> {
        let iface = self
            .config
            .dataplane_iface
            .as_ref()
            .ok_or(FError::NotFound)?;
        let addresses = self.get_iface_addresses(iface.clone()).await?;
        Ok(Interface {
            if_name: iface.to_string(),
            kind: InterfaceKind::ETHERNET,
            addresses,
            phy_address: None,
        })
    }

    fn get_domain_socket_locator(&self) -> String {
        self.config.zfilelocator.clone()
    }

    fn get_path(&self) -> Box<std::path::Path> {
        self.config.path.clone()
    }

    fn get_run_path(&self) -> Box<std::path::Path> {
        self.config.run_path.clone()
    }

    fn generate_random_interface_name(&self) -> String {
        let iface: String = thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        iface
    }

    fn generate_random_netns_name(&self) -> String {
        let ns: String = thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        format!("ns-{}", ns)
    }

    fn generate_random_nft_table_name(&self) -> String {
        let tab: String = thread_rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect();
        format!("table{}", tab)
    }

    async fn add_netns(&self, ns_name: String) -> FResult<()> {
        log::trace!("add_netns {}", ns_name);
        NetlinkNetworkNamespace::add(ns_name)
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))
    }

    async fn del_netns(&self, ns_name: String) -> FResult<()> {
        log::trace!("del_netns {}", ns_name);
        NetlinkNetworkNamespace::del(ns_name)
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))
    }

    async fn create_bridge(&self, br_name: String) -> FResult<()> {
        log::trace!("create_bridge {}", br_name);
        let mut backoff = 100;
        loop {
            let mut state = self.state.write().await;
            let res = state
                .nl_handler
                .link()
                .add()
                .bridge(br_name.clone())
                .execute()
                .await;
            drop(state);

            match res {
                Ok(_) => return Ok(()),
                Err(nlError::NetlinkError(nl)) => {
                    if nl.code == -16 {
                        task::sleep(Duration::from_millis(backoff)).await;
                    } else {
                        return Err(FError::NetworkingError(format!("{}", nl)));
                    }
                }
                Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
            }
            backoff *= 2;
            if backoff > 5000 {
                return Err(FError::NetworkingError("Timeout".to_string()));
            }
        }
    }

    async fn create_veth(&self, iface_i: String, iface_e: String) -> FResult<()> {
        log::trace!("create_veth {} {}", iface_i, iface_e);

        let mut backoff = 100;
        loop {
            let mut state = self.state.write().await;

            let res = state
                .nl_handler
                .link()
                .add()
                .veth(iface_i.clone(), iface_e.clone())
                .execute()
                .await;
            drop(state);
            match res {
                Ok(_) => return Ok(()),
                Err(nlError::NetlinkError(nl)) => {
                    if nl.code == -16 {
                        task::sleep(Duration::from_millis(backoff)).await;
                    } else {
                        return Err(FError::NetworkingError(format!("{}", nl)));
                    }
                }
                Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
            }
            backoff *= 2;
            if backoff > 5000 {
                return Err(FError::NetworkingError("Timeout".to_string()));
            }
        }
    }

    async fn create_vlan(&self, iface: String, dev: String, tag: u16) -> FResult<()> {
        let mut state = self.state.write().await;
        log::trace!("create_vlan {} {} {}", iface, dev, tag);
        let mut backoff = 100;

        let mut links = state.nl_handler.link().get().set_name_filter(dev).execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .add()
                    .vlan(iface.clone(), link.header.index, tag)
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn create_mcast_vxlan(
        &self,
        iface: String,
        dev: String,
        vni: u32,
        mcast_addr: IPAddress,
        port: u16,
    ) -> FResult<()> {
        log::trace!(
            "create_mcast_vxlan {} {} {} {} {}",
            iface,
            dev,
            vni,
            mcast_addr,
            port
        );
        let mut backoff = 100;
        let mut state = self.state.write().await;

        let mut links = state.nl_handler.link().get().set_name_filter(dev).execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            loop {
                let vxlan = state
                    .nl_handler
                    .link()
                    .add()
                    .vxlan(iface.clone(), vni)
                    .link(link.header.index);

                let vxlan = match mcast_addr {
                    IPAddress::V4(v4) => vxlan.group(v4),
                    IPAddress::V6(v6) => vxlan.group6(v6),
                };

                let res = vxlan.port(port).execute().await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn create_ptp_vxlan(
        &self,
        iface: String,
        dev: String,
        vni: u32,
        local_addr: IPAddress,
        remote_addr: IPAddress,
        port: u16,
    ) -> FResult<()> {
        log::trace!(
            "create_ptp_vxlan {} {} {} {} {} {}",
            iface,
            dev,
            vni,
            local_addr,
            remote_addr,
            port
        );
        let mut backoff = 100;
        let mut state = self.state.write().await;
        let mut links = state.nl_handler.link().get().set_name_filter(dev).execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            loop {
                let vxlan = state
                    .nl_handler
                    .link()
                    .add()
                    .vxlan(iface.clone(), vni)
                    .link(link.header.index);

                let vxlan = match local_addr {
                    IPAddress::V4(v4) => vxlan.local(v4),
                    IPAddress::V6(v6) => vxlan.local6(v6),
                };

                let vxlan = match remote_addr {
                    IPAddress::V4(v4) => vxlan.remote(v4),
                    IPAddress::V6(v6) => vxlan.remote6(v6),
                };
                let res = vxlan.port(port).execute().await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn del_iface(&self, iface: String) -> FResult<()> {
        log::trace!("del_iface {}", iface);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .del(link.header.index)
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn set_iface_master(&self, iface: String, master: String) -> FResult<()> {
        log::trace!("set_iface_master {} {}", iface, master);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut masters = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(master)
                .execute();
            if let Some(master) = masters
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                let mut backoff = 100;
                loop {
                    let res = state
                        .nl_handler
                        .link()
                        .set(link.header.index)
                        .master(master.header.index)
                        .execute()
                        .await;
                    match res {
                        Ok(_) => return Ok(()),
                        Err(nlError::NetlinkError(nl)) => {
                            if nl.code == -16 {
                                task::sleep(Duration::from_millis(backoff)).await;
                            } else {
                                return Err(FError::NetworkingError(format!("{}", nl)));
                            }
                        }
                        Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                    }
                    backoff *= 2;
                    if backoff > 5000 {
                        return Err(FError::NetworkingError("Timeout".to_string()));
                    }
                }
            } else {
                log::error!("set_iface_master master not found");
                Err(FError::NotFound)
            }
        } else {
            log::error!("set_iface_master iface not found");
            Err(FError::NotFound)
        }
    }

    async fn del_iface_master(&self, iface: String) -> FResult<()> {
        log::trace!("del_iface_master {}", iface);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .nomaster()
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            log::error!("del_iface_master iface not found");
            Err(FError::NotFound)
        }
    }

    async fn add_iface_address(&self, iface: String, addr: IPAddress, prefix: u8) -> FResult<()> {
        log::trace!("add_iface_address {} {} {}", iface, addr, prefix);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .address()
                    .add(link.header.index, addr, prefix)
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn del_iface_address(&self, iface: String, addr: IPAddress) -> FResult<()> {
        log::trace!("del_iface_address {} {}", iface, addr);
        let mut state = self.state.write().await;
        use netlink_packet_route::rtnl::address::nlas::Nla;
        use netlink_packet_route::rtnl::address::AddressMessage;
        let octets = match addr {
            IPAddress::V4(a) => a.octets().to_vec(),
            IPAddress::V6(a) => a.octets().to_vec(),
        };
        let mut nl_addresses = Vec::new();
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface.clone())
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut addresses = state
                .nl_handler
                .address()
                .get()
                .set_link_index_filter(link.header.index)
                .execute();
            while let Some(msg) = addresses
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                for nla in &msg.nlas {
                    match nla {
                        Nla::Address(nl_addr) => {
                            nl_addresses.push((msg.header.clone(), nl_addr.clone()))
                        }
                        _ => continue,
                    }
                }
            }
            match nl_addresses.into_iter().find(|(_, x)| *x == octets) {
                Some((hdr, addr)) => {
                    let msg = AddressMessage {
                        header: hdr,
                        nlas: vec![Nla::Address(addr)],
                    };
                    let mut backoff = 100;
                    loop {
                        let res = state.nl_handler.address().del(msg.clone()).execute().await;
                        match res {
                            Ok(_) => return Ok(()),
                            Err(nlError::NetlinkError(nl)) => {
                                if nl.code == -16 {
                                    task::sleep(Duration::from_millis(backoff)).await;
                                } else {
                                    return Err(FError::NetworkingError(format!("{}", nl)));
                                }
                            }
                            Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                        }
                        backoff *= 2;
                        if backoff > 5000 {
                            return Err(FError::NetworkingError("Timeout".to_string()));
                        }
                    }
                }
                None => Err(FError::NotFound),
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn get_iface_addresses(&self, iface: String) -> FResult<Vec<IPAddress>> {
        log::trace!("get_iface_addresses {}", iface);
        let mut state = self.state.write().await;
        use netlink_packet_route::rtnl::address::nlas::Nla;
        use netlink_packet_route::rtnl::address::AddressMessage;
        let mut nl_addresses = Vec::new();
        let mut f_addresses: Vec<IPAddress> = Vec::new();
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface.clone())
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut addresses = state
                .nl_handler
                .address()
                .get()
                .set_link_index_filter(link.header.index)
                .execute();
            while let Some(msg) = addresses
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                for nla in &msg.nlas {
                    match nla {
                        Nla::Address(nl_addr) => {
                            nl_addresses.push((msg.header.clone(), nl_addr.clone()))
                        }
                        _ => continue,
                    }
                }
            }
            for (_, x) in nl_addresses {
                if x.len() == 4 {
                    let octects: [u8; 4] = [x[0], x[1], x[2], x[3]];
                    f_addresses.push(IPAddress::from(octects))
                }
                if x.len() == 16 {
                    let octects: [u8; 16] = [
                        x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7], x[8], x[9], x[10], x[11],
                        x[12], x[13], x[14], x[15],
                    ];
                    f_addresses.push(IPAddress::from(octects))
                }
            }
            Ok(f_addresses)
        } else {
            Err(FError::NotFound)
        }
    }

    async fn set_iface_name(&self, iface: String, new_name: String) -> FResult<()> {
        log::trace!("set_iface_name {} {}", iface, new_name);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .name(new_name.clone())
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn set_iface_mac(&self, iface: String, address: Vec<u8>) -> FResult<()> {
        log::trace!("set_iface_mac {} {:?}", iface, address);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .address(address.clone())
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn set_iface_ns(&self, iface: String, netns: String) -> FResult<()> {
        log::trace!("set_iface_ns {} {}", iface, netns);
        const NETNS_PATH: &str = "/run/netns/";
        let netns = format!("{}{}", NETNS_PATH, netns);
        let mut state = self.state.write().await;
        let nsfile = std::fs::File::open(netns)?;
        let raw_fd = nsfile.into_raw_fd();
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .setns_by_fd(raw_fd)
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn set_iface_default_ns(&self, iface: String) -> FResult<()> {
        log::trace!("set_iface_default_ns {}", iface);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .setns_by_pid(0)
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn set_iface_up(&self, iface: String) -> FResult<()> {
        log::trace!("set_iface_up {}", iface);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .up()
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn set_iface_down(&self, iface: String) -> FResult<()> {
        log::trace!("set_iface_down {}", iface);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            let mut backoff = 100;
            loop {
                let res = state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .down()
                    .execute()
                    .await;
                match res {
                    Ok(_) => return Ok(()),
                    Err(nlError::NetlinkError(nl)) => {
                        if nl.code == -16 {
                            task::sleep(Duration::from_millis(backoff)).await;
                        } else {
                            return Err(FError::NetworkingError(format!("{}", nl)));
                        }
                    }
                    Err(e) => return Err(FError::NetworkingError(format!("{}", e))),
                }
                backoff *= 2;
                if backoff > 5000 {
                    return Err(FError::NetworkingError("Timeout".to_string()));
                }
            }
        } else {
            Err(FError::NotFound)
        }
    }

    async fn iface_exists(&self, iface: String) -> FResult<bool> {
        log::trace!("iface_exists {}", iface);
        let mut state = self.state.write().await;
        let mut links = state
            .nl_handler
            .link()
            .get()
            .set_name_filter(iface)
            .execute();
        if let Some(link) = links
            .try_next()
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?
        {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn spawn_dnsmasq(&self, config_file: String) -> FResult<Child> {
        let child = Command::new("dnsmasq")
            .arg("-C")
            .arg(config_file)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
        Ok(child)
    }

    async fn create_dnsmasq_config(
        &self,
        iface: &str,
        pid_file: &str,
        lease_file: &str,
        log_file: &str,
        dhcp_start: IPAddress,
        dhcp_end: IPAddress,
        default_gw: IPAddress,
        default_dns: IPAddress,
    ) -> FResult<String> {
        log::trace!(
            "create_dnsmasq_config {} {} {} {} {} {} {}",
            iface,
            pid_file,
            lease_file,
            dhcp_start,
            dhcp_end,
            default_gw,
            default_dns,
        );
        let mut context = Context::new();
        let template_path = self
            .get_path()
            .join("*.conf")
            .to_str()
            .ok_or(FError::EncodingError)?
            .to_string();
        let templates =
            Tera::new(&template_path).map_err(|e| FError::NetworkingError(format!("{}", e)))?;
        context.insert("dhcp_interface", iface);
        context.insert("lease_file", lease_file);
        context.insert("dhcp_pid", pid_file);
        context.insert("dhcp_log", log_file);
        context.insert("dhcp_start", &format!("{}", dhcp_start));
        context.insert("dhcp_end", &format!("{}", dhcp_end));
        context.insert("default_gw", &format!("{}", default_gw));
        context.insert("default_dns", &format!("{}", default_dns));

        match templates.render("dnsmasq.conf", &context) {
            Ok(t) => Ok(t),
            Err(e) => {
                log::error!("Parsing error(s): {} {}", e, e.source().unwrap());
                Err(FError::NetworkingError(format!(
                    "{} {}",
                    e,
                    e.source().unwrap()
                )))
            }
        }
    }

    async fn configure_nat(&self, net: IpNetwork, iface: &str) -> FResult<String> {
        let table_name = self.generate_random_nft_table_name();
        let chain_name = String::from("postrouting");
        // Create a batch. This is used to store all the netlink messages we will later send.
        // Creating a new batch also automatically writes the initial batch begin message needed
        // to tell netlink this is a single transaction that might arrive over multiple netlink packets.
        let mut batch = Batch::new();
        // Create a netfilter table operating on both IPv4 and IPv6 (ProtoFamily::Inet)
        let table = Table::new(
            &CString::new(table_name.clone())
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?,
            ProtoFamily::Inet,
        );
        // Add the table to the batch with the `MsgType::Add` type, thus instructing netfilter to add
        // this table under its `ProtoFamily::Inet` ruleset.
        batch.add(&table, nftnl::MsgType::Add);

        // Create a chain under the table we created above.
        let mut chain = Chain::new(
            &CString::new(chain_name).map_err(|e| FError::NetworkingError(format!("{}", e)))?,
            &table,
        );

        // Hook the chains to the input and output event hooks, with highest priority (priority zero).
        // See the `Chain::set_hook` documentation for details.
        chain.set_hook(nftnl::Hook::PostRouting, 0);
        // Set the chain type.
        // See the `Chain::set_type` documentation for details.
        chain.set_type(nftnl::ChainType::Nat);

        // Add the two chains to the batch with the `MsgType` to tell netfilter to create the chains
        // under the table.
        batch.add(&chain, nftnl::MsgType::Add);

        // Create a new rule object under the input chain.
        let mut natting_rule = Rule::new(&chain);

        // Lookup the interface index of the default gw interface.
        let iface_index = iface_index(iface)?;
        //Type of payload is source address
        natting_rule.add_expr(&nft_expr!(payload ipv4 saddr));

        //netmask of the network
        natting_rule.add_expr(&nft_expr!(bitwise mask net.mask(), xor 0u32));

        //comparing ip portion of the address
        natting_rule.add_expr(&nft_expr!(cmp == net.ip()));

        // passing the index of output interface oif
        natting_rule.add_expr(&nft_expr!(meta oif));

        //use interface with this index
        natting_rule.add_expr(&nft_expr!(cmp == iface_index));

        // Add masquerading
        natting_rule.add_expr(&nft_expr!(masquerade));

        // Add the rule to the batch.
        batch.add(&natting_rule, nftnl::MsgType::Add);

        // === FINALIZE THE TRANSACTION AND SEND THE DATA TO NETFILTER ===

        // Finalize the batch. This means the batch end message is written into the batch, telling
        // netfilter the we reached the end of the transaction message. It's also converted to a type
        // that implements `IntoIterator<Item = &'a [u8]>`, thus allowing us to get the raw netlink data
        // out so it can be sent over a netlink socket to netfilter.
        let finalized_batch = batch.finalize();

        fn send_and_process(batch: &FinalizedBatch) -> FResult<()> {
            // Create a netlink socket to netfilter.
            let socket = mnl::Socket::new(mnl::Bus::Netfilter)?;
            // Send all the bytes in the batch.
            socket.send_all(batch)?;
            // Try to parse the messages coming back from netfilter. This part is still very unclear.
            let portid = socket.portid();
            let mut buffer = vec![0; nftnl::nft_nlmsg_maxsize() as usize];
            let very_unclear_what_this_is_for = 2;
            while let Some(message) = socket_recv(&socket, &mut buffer[..])? {
                match mnl::cb_run(message, very_unclear_what_this_is_for, portid)? {
                    mnl::CbResult::Stop => {
                        break;
                    }
                    mnl::CbResult::Ok => (),
                }
            }
            Ok(())
        }

        fn socket_recv<'a>(socket: &mnl::Socket, buf: &'a mut [u8]) -> FResult<Option<&'a [u8]>> {
            let ret = socket.recv(buf)?;
            if ret > 0 {
                Ok(Some(&buf[..ret]))
            } else {
                Ok(None)
            }
        }

        // Look up the interface index for a given interface name.
        fn iface_index(name: &str) -> FResult<libc::c_uint> {
            let c_name =
                CString::new(name).map_err(|e| FError::NetworkingError(format!("{}", e)))?;
            let index = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
            if index == 0 {
                Err(FError::from(std::io::Error::last_os_error()))
            } else {
                Ok(index)
            }
        }

        send_and_process(&finalized_batch)?;
        Ok(table_name)
    }

    async fn clean_nat(&self, table_name: String) -> FResult<()> {
        // Create a batch. This is used to store all the netlink messages we will later send.
        // Creating a new batch also automatically writes the initial batch begin message needed
        // to tell netlink this is a single transaction that might arrive over multiple netlink packets.
        let mut batch = Batch::new();
        // Create a netfilter table operating on both IPv4 and IPv6 (ProtoFamily::Inet)
        let table = Table::new(
            &CString::new(table_name).map_err(|e| FError::NetworkingError(format!("{}", e)))?,
            ProtoFamily::Inet,
        );
        // Add the table to the batch with the `MsgType::Del` type, thus instructing netfilter to remove
        // this table under its `ProtoFamily::Inet` ruleset.
        batch.add(&table, nftnl::MsgType::Del);

        // === FINALIZE THE TRANSACTION AND SEND THE DATA TO NETFILTER ===

        // Finalize the batch. This means the batch end message is written into the batch, telling
        // netfilter the we reached the end of the transaction message. It's also converted to a type
        // that implements `IntoIterator<Item = &'a [u8]>`, thus allowing us to get the raw netlink data
        // out so it can be sent over a netlink socket to netfilter.
        let finalized_batch = batch.finalize();

        fn send_and_process(batch: &FinalizedBatch) -> FResult<()> {
            // Create a netlink socket to netfilter.
            let socket = mnl::Socket::new(mnl::Bus::Netfilter)?;
            // Send all the bytes in the batch.
            socket.send_all(batch)?;
            // Try to parse the messages coming back from netfilter. This part is still very unclear.
            let portid = socket.portid();
            let mut buffer = vec![0; nftnl::nft_nlmsg_maxsize() as usize];
            let very_unclear_what_this_is_for = 2;
            while let Some(message) = socket_recv(&socket, &mut buffer[..])? {
                match mnl::cb_run(message, very_unclear_what_this_is_for, portid)? {
                    mnl::CbResult::Stop => {
                        break;
                    }
                    mnl::CbResult::Ok => (),
                }
            }
            Ok(())
        }

        fn socket_recv<'a>(socket: &mnl::Socket, buf: &'a mut [u8]) -> FResult<Option<&'a [u8]>> {
            let ret = socket.recv(buf)?;
            if ret > 0 {
                Ok(Some(&buf[..ret]))
            } else {
                Ok(None)
            }
        }

        send_and_process(&finalized_batch)?;
        Ok(())
    }
}
