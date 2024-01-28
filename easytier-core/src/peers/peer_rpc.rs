use std::sync::Arc;

use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use rkyv::Deserialize;
use tarpc::{server::Channel, transport::channel::UnboundedChannel};
use tokio::{
    sync::mpsc::{self, UnboundedSender},
    task::JoinSet,
};
use tokio_util::bytes::Bytes;
use tracing::Instrument;

use crate::{common::error::Error, peers::packet::Packet};

use super::packet::{CtrlPacketBody, PacketBody};

type PeerRpcServiceId = u32;

#[async_trait::async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait PeerRpcManagerTransport: Send + Sync + 'static {
    fn my_peer_id(&self) -> uuid::Uuid;
    async fn send(&self, msg: Bytes, dst_peer_id: &uuid::Uuid) -> Result<(), Error>;
    async fn recv(&self) -> Result<Bytes, Error>;
}

type PacketSender = UnboundedSender<Packet>;

struct PeerRpcEndPoint {
    peer_id: uuid::Uuid,
    packet_sender: PacketSender,
    tasks: JoinSet<()>,
}

type PeerRpcEndPointCreator = Box<dyn Fn(uuid::Uuid) -> PeerRpcEndPoint + Send + Sync + 'static>;
#[derive(Hash, Eq, PartialEq, Clone)]
struct PeerRpcClientCtxKey(uuid::Uuid, PeerRpcServiceId);

// handle rpc request from one peer
pub struct PeerRpcManager {
    service_map: Arc<DashMap<PeerRpcServiceId, PacketSender>>,
    tasks: JoinSet<()>,
    tspt: Arc<Box<dyn PeerRpcManagerTransport>>,

    service_registry: Arc<DashMap<PeerRpcServiceId, PeerRpcEndPointCreator>>,
    peer_rpc_endpoints: Arc<DashMap<(uuid::Uuid, PeerRpcServiceId), PeerRpcEndPoint>>,

    client_resp_receivers: Arc<DashMap<PeerRpcClientCtxKey, PacketSender>>,
}

impl std::fmt::Debug for PeerRpcManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerRpcManager")
            .field("node_id", &self.tspt.my_peer_id())
            .finish()
    }
}

#[derive(Debug)]
struct TaRpcPacketInfo {
    from_peer: uuid::Uuid,
    to_peer: uuid::Uuid,
    service_id: PeerRpcServiceId,
    is_req: bool,
    content: Vec<u8>,
}

impl PeerRpcManager {
    pub fn new(tspt: impl PeerRpcManagerTransport) -> Self {
        Self {
            service_map: Arc::new(DashMap::new()),
            tasks: JoinSet::new(),
            tspt: Arc::new(Box::new(tspt)),

            service_registry: Arc::new(DashMap::new()),
            peer_rpc_endpoints: Arc::new(DashMap::new()),

            client_resp_receivers: Arc::new(DashMap::new()),
        }
    }

    pub fn run_service<S, Req>(self: &Self, service_id: PeerRpcServiceId, s: S) -> ()
    where
        S: tarpc::server::Serve<Req> + Clone + Send + Sync + 'static,
        Req: Send + 'static + serde::Serialize + for<'a> serde::Deserialize<'a>,
        S::Resp:
            Send + std::fmt::Debug + 'static + serde::Serialize + for<'a> serde::Deserialize<'a>,
        S::Fut: Send + 'static,
    {
        let tspt = self.tspt.clone();
        let creator = Box::new(move |peer_id: uuid::Uuid| {
            let mut tasks = JoinSet::new();
            let (packet_sender, mut packet_receiver) = mpsc::unbounded_channel::<Packet>();
            let (mut client_transport, server_transport) = tarpc::transport::channel::unbounded();
            let server = tarpc::server::BaseChannel::with_defaults(server_transport);

            let my_peer_id_clone = tspt.my_peer_id();
            let peer_id_clone = peer_id.clone();

            let o = server.execute(s.clone());
            tasks.spawn(o);

            let tspt = tspt.clone();
            tasks.spawn(async move {
                let mut cur_req_uuid = None;
                loop {
                    tokio::select! {
                        Some(resp) = client_transport.next() => {
                            tracing::trace!(resp = ?resp, "recv packet from client");
                            if resp.is_err() {
                                tracing::warn!(err = ?resp.err(),
                                    "[PEER RPC MGR] client_transport in server side got channel error, ignore it.");
                                continue;
                            }
                            let resp = resp.unwrap();

                            if cur_req_uuid.is_none() {
                                tracing::error!("[PEER RPC MGR] cur_req_uuid is none, ignore this resp");
                                continue;
                            }

                            let serialized_resp = bincode::serialize(&resp);
                            if serialized_resp.is_err() {
                                tracing::error!(error = ?serialized_resp.err(), "serialize resp failed");
                                continue;
                            }

                            let msg = Packet::new_tarpc_packet(
                                tspt.my_peer_id(),
                                cur_req_uuid.take().unwrap(),
                                service_id,
                                false,
                                serialized_resp.unwrap(),
                            );

                            if let Err(e) = tspt.send(msg.into(), &peer_id).await {
                                tracing::error!(error = ?e, peer_id = ?peer_id, service_id = ?service_id, "send resp to peer failed");
                            }
                        }
                        Some(packet) = packet_receiver.recv() => {
                            let info = Self::parse_rpc_packet(&packet);
                            if let Err(e) = info {
                                tracing::error!(error = ?e, packet = ?packet, "parse rpc packet failed");
                                continue;
                            }
                            let info = info.unwrap();

                            assert_eq!(info.service_id, service_id);
                            cur_req_uuid = Some(packet.from_peer.clone().into());

                            tracing::trace!("recv packet from peer, packet: {:?}", packet);

                            let decoded_ret = bincode::deserialize(&info.content.as_slice());
                            if let Err(e) = decoded_ret {
                                tracing::error!(error = ?e, "decode rpc packet failed");
                                continue;
                            }
                            let decoded: tarpc::ClientMessage<Req> = decoded_ret.unwrap();

                            if let Err(e) = client_transport.send(decoded).await {
                                tracing::error!(error = ?e, "send to req to client transport failed");
                            }
                        }
                        else => {
                            tracing::warn!("[PEER RPC MGR] service runner destroy, peer_id: {}, service_id: {}", peer_id, service_id);
                        }
                    }
                }
            }.instrument(tracing::info_span!("service_runner", my_id = ?my_peer_id_clone, peer_id = ?peer_id_clone, service_id = ?service_id)));

            tracing::info!(
                "[PEER RPC MGR] create new service endpoint for peer {}, service {}",
                peer_id,
                service_id
            );

            return PeerRpcEndPoint {
                peer_id,
                packet_sender,
                tasks,
            };
            // let resp = client_transport.next().await;
        });

        if let Some(_) = self.service_registry.insert(service_id, creator) {
            panic!(
                "[PEER RPC MGR] service {} is already registered",
                service_id
            );
        }

        log::info!(
            "[PEER RPC MGR] register service {} succeed, my_node_id {}",
            service_id,
            self.tspt.my_peer_id()
        )
    }

    fn parse_rpc_packet(packet: &Packet) -> Result<TaRpcPacketInfo, Error> {
        match &packet.body {
            PacketBody::Ctrl(CtrlPacketBody::TaRpc(id, is_req, body)) => Ok(TaRpcPacketInfo {
                from_peer: packet.from_peer.clone().into(),
                to_peer: packet.to_peer.clone().unwrap().into(),
                service_id: *id,
                is_req: *is_req,
                content: body.clone(),
            }),
            _ => Err(Error::ShellCommandError("invalid packet".to_owned())),
        }
    }

    pub fn run(&self) {
        let tspt = self.tspt.clone();
        let service_registry = self.service_registry.clone();
        let peer_rpc_endpoints = self.peer_rpc_endpoints.clone();
        let client_resp_receivers = self.client_resp_receivers.clone();
        tokio::spawn(async move {
            loop {
                let o = tspt.recv().await.unwrap();
                let packet = Packet::decode(&o);
                let packet: Packet = packet.deserialize(&mut rkyv::Infallible).unwrap();
                let info = Self::parse_rpc_packet(&packet).unwrap();

                if info.is_req {
                    if !service_registry.contains_key(&info.service_id) {
                        log::warn!(
                            "service {} not found, my_node_id: {}",
                            info.service_id,
                            tspt.my_peer_id()
                        );
                        continue;
                    }

                    let endpoint = peer_rpc_endpoints
                        .entry((info.to_peer, info.service_id))
                        .or_insert_with(|| {
                            service_registry.get(&info.service_id).unwrap()(info.from_peer)
                        });

                    endpoint.packet_sender.send(packet).unwrap();
                } else {
                    if let Some(a) = client_resp_receivers
                        .get(&PeerRpcClientCtxKey(info.from_peer, info.service_id))
                    {
                        log::trace!("recv resp: {:?}", packet);
                        if let Err(e) = a.send(packet) {
                            tracing::error!(error = ?e, "send resp to client failed");
                        }
                    } else {
                        log::warn!("client resp receiver not found, info: {:?}", info);
                    }
                }
            }
        });
    }

    #[tracing::instrument(skip(f))]
    pub async fn do_client_rpc_scoped<CM, Req, RpcRet, Fut>(
        &self,
        service_id: PeerRpcServiceId,
        dst_peer_id: uuid::Uuid,
        f: impl FnOnce(UnboundedChannel<CM, Req>) -> Fut,
    ) -> RpcRet
    where
        CM: serde::Serialize + for<'a> serde::Deserialize<'a> + Send + Sync + 'static,
        Req: serde::Serialize + for<'a> serde::Deserialize<'a> + Send + Sync + 'static,
        Fut: std::future::Future<Output = RpcRet>,
    {
        let mut tasks = JoinSet::new();
        let (packet_sender, mut packet_receiver) = mpsc::unbounded_channel::<Packet>();
        let (client_transport, server_transport) =
            tarpc::transport::channel::unbounded::<CM, Req>();

        let (mut server_s, mut server_r) = server_transport.split();

        let tspt = self.tspt.clone();
        tasks.spawn(async move {
            while let Some(a) = server_r.next().await {
                if a.is_err() {
                    tracing::error!(error = ?a.err(), "channel error");
                    continue;
                }

                let a = bincode::serialize(&a.unwrap());
                if a.is_err() {
                    tracing::error!(error = ?a.err(), "bincode serialize failed");
                    continue;
                }

                let a = Packet::new_tarpc_packet(
                    tspt.my_peer_id(),
                    dst_peer_id,
                    service_id,
                    true,
                    a.unwrap(),
                );

                if let Err(e) = tspt.send(a.into(), &dst_peer_id).await {
                    tracing::error!(error = ?e, dst_peer_id = ?dst_peer_id, "send to peer failed");
                }
            }

            tracing::warn!("[PEER RPC MGR] server trasport read aborted");
        });

        tasks.spawn(async move {
            while let Some(packet) = packet_receiver.recv().await {
                tracing::trace!("tunnel recv: {:?}", packet);

                let info = PeerRpcManager::parse_rpc_packet(&packet);
                if let Err(e) = info {
                    tracing::error!(error = ?e, "parse rpc packet failed");
                    continue;
                }

                let decoded = bincode::deserialize(&info.unwrap().content.as_slice());
                if let Err(e) = decoded {
                    tracing::error!(error = ?e, "decode rpc packet failed");
                    continue;
                }

                if let Err(e) = server_s.send(decoded.unwrap()).await {
                    tracing::error!(error = ?e, "send to rpc server channel failed");
                }
            }

            tracing::warn!("[PEER RPC MGR] server packet read aborted");
        });

        let _insert_ret = self
            .client_resp_receivers
            .insert(PeerRpcClientCtxKey(dst_peer_id, service_id), packet_sender);

        f(client_transport).await
    }

    pub fn my_peer_id(&self) -> uuid::Uuid {
        self.tspt.my_peer_id()
    }
}

#[cfg(test)]
mod tests {
    use futures::{SinkExt, StreamExt};
    use tokio_util::bytes::Bytes;

    use crate::{
        common::error::Error,
        peers::{
            peer_rpc::PeerRpcManager,
            tests::{connect_peer_manager, create_mock_peer_manager, wait_route_appear},
        },
        tunnels::{self, ring_tunnel::create_ring_tunnel_pair},
    };

    use super::PeerRpcManagerTransport;

    #[tarpc::service]
    pub trait TestRpcService {
        async fn hello(s: String) -> String;
    }

    #[derive(Clone)]
    struct MockService {
        prefix: String,
    }

    #[tarpc::server]
    impl TestRpcService for MockService {
        async fn hello(self, _: tarpc::context::Context, s: String) -> String {
            format!("{} {}", self.prefix, s)
        }
    }

    #[tokio::test]
    async fn peer_rpc_basic_test() {
        struct MockTransport {
            tunnel: Box<dyn tunnels::Tunnel>,
            my_peer_id: uuid::Uuid,
        }

        #[async_trait::async_trait]
        impl PeerRpcManagerTransport for MockTransport {
            fn my_peer_id(&self) -> uuid::Uuid {
                self.my_peer_id
            }
            async fn send(&self, msg: Bytes, _dst_peer_id: &uuid::Uuid) -> Result<(), Error> {
                println!("rpc mgr send: {:?}", msg);
                self.tunnel.pin_sink().send(msg).await.unwrap();
                Ok(())
            }
            async fn recv(&self) -> Result<Bytes, Error> {
                let ret = self.tunnel.pin_stream().next().await.unwrap();
                println!("rpc mgr recv: {:?}", ret);
                return ret.map(|v| v.freeze()).map_err(|_| Error::Unknown);
            }
        }

        let (ct, st) = create_ring_tunnel_pair();

        let server_rpc_mgr = PeerRpcManager::new(MockTransport {
            tunnel: st,
            my_peer_id: uuid::Uuid::new_v4(),
        });
        server_rpc_mgr.run();
        let s = MockService {
            prefix: "hello".to_owned(),
        };
        server_rpc_mgr.run_service(1, s.serve());

        let client_rpc_mgr = PeerRpcManager::new(MockTransport {
            tunnel: ct,
            my_peer_id: uuid::Uuid::new_v4(),
        });
        client_rpc_mgr.run();

        let ret = client_rpc_mgr
            .do_client_rpc_scoped(1, server_rpc_mgr.my_peer_id(), |c| async {
                let c = TestRpcServiceClient::new(tarpc::client::Config::default(), c).spawn();
                let ret = c.hello(tarpc::context::current(), "abc".to_owned()).await;
                ret
            })
            .await;

        println!("ret: {:?}", ret);
        assert_eq!(ret.unwrap(), "hello abc");
    }

    #[tokio::test]
    async fn test_rpc_with_peer_manager() {
        let peer_mgr_a = create_mock_peer_manager().await;
        let peer_mgr_b = create_mock_peer_manager().await;
        connect_peer_manager(peer_mgr_a.clone(), peer_mgr_b.clone()).await;
        wait_route_appear(peer_mgr_a.clone(), peer_mgr_b.my_node_id())
            .await
            .unwrap();

        assert_eq!(peer_mgr_a.get_peer_map().list_peers().await.len(), 1);
        assert_eq!(
            peer_mgr_a.get_peer_map().list_peers().await[0],
            peer_mgr_b.my_node_id()
        );

        let s = MockService {
            prefix: "hello".to_owned(),
        };
        peer_mgr_b.get_peer_rpc_mgr().run_service(1, s.serve());

        let ip_list = peer_mgr_a
            .get_peer_rpc_mgr()
            .do_client_rpc_scoped(1, peer_mgr_b.my_node_id(), |c| async {
                let c = TestRpcServiceClient::new(tarpc::client::Config::default(), c).spawn();
                let ret = c.hello(tarpc::context::current(), "abc".to_owned()).await;
                ret
            })
            .await;

        println!("ip_list: {:?}", ip_list);
        assert_eq!(ip_list.as_ref().unwrap(), "hello abc");
    }

    #[tokio::test]
    async fn test_multi_service_with_peer_manager() {
        let peer_mgr_a = create_mock_peer_manager().await;
        let peer_mgr_b = create_mock_peer_manager().await;
        connect_peer_manager(peer_mgr_a.clone(), peer_mgr_b.clone()).await;
        wait_route_appear(peer_mgr_a.clone(), peer_mgr_b.my_node_id())
            .await
            .unwrap();

        assert_eq!(peer_mgr_a.get_peer_map().list_peers().await.len(), 1);
        assert_eq!(
            peer_mgr_a.get_peer_map().list_peers().await[0],
            peer_mgr_b.my_node_id()
        );

        let s = MockService {
            prefix: "hello_a".to_owned(),
        };
        peer_mgr_b.get_peer_rpc_mgr().run_service(1, s.serve());
        let b = MockService {
            prefix: "hello_b".to_owned(),
        };
        peer_mgr_b.get_peer_rpc_mgr().run_service(2, b.serve());

        let ip_list = peer_mgr_a
            .get_peer_rpc_mgr()
            .do_client_rpc_scoped(1, peer_mgr_b.my_node_id(), |c| async {
                let c = TestRpcServiceClient::new(tarpc::client::Config::default(), c).spawn();
                let ret = c.hello(tarpc::context::current(), "abc".to_owned()).await;
                ret
            })
            .await;

        assert_eq!(ip_list.as_ref().unwrap(), "hello_a abc");

        let ip_list = peer_mgr_a
            .get_peer_rpc_mgr()
            .do_client_rpc_scoped(2, peer_mgr_b.my_node_id(), |c| async {
                let c = TestRpcServiceClient::new(tarpc::client::Config::default(), c).spawn();
                let ret = c.hello(tarpc::context::current(), "abc".to_owned()).await;
                ret
            })
            .await;

        assert_eq!(ip_list.as_ref().unwrap(), "hello_b abc");
    }
}