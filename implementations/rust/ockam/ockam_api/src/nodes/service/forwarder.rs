use std::sync::Arc;

use minicbor::Decoder;
use ockam::compat::asynchronous::RwLock;
use ockam::compat::sync::Mutex;
use ockam::identity::Identifier;
use ockam::remote::{RemoteForwarder, RemoteForwarderOptions};
use ockam::Result;
use ockam_core::api::{Error, Request, Response, ResponseBuilder};
use ockam_core::AsyncTryClone;
use ockam_multiaddr::MultiAddr;
use ockam_node::tokio::time::timeout;
use ockam_node::Context;

use crate::error::ApiError;
use crate::local_multiaddr_to_route;
use crate::nodes::connection::{Connection, ConnectionInstance};
use crate::nodes::models::forwarder::{CreateForwarder, ForwarderInfo};
use crate::session::sessions::{Replacer, Session};
use crate::session::sessions::{MAX_CONNECT_TIME, MAX_RECOVERY_TIME};

use super::{NodeManager, NodeManagerWorker};

impl NodeManagerWorker {
    pub(super) async fn create_forwarder_response(
        &self,
        ctx: &Context,
        req: &Request,
        dec: &mut Decoder<'_>,
    ) -> Result<Vec<u8>> {
        let req_body: CreateForwarder = dec.decode()?;
        match self.create_forwarder(ctx, req_body).await {
            Ok(body) => Ok(Response::ok(req.id()).body(body).to_vec()?),
            Err(err) => {
                let err = Error::new(req.path())
                    .with_message("Failed to create forwarder")
                    .with_cause(Error::new(req.path()).with_message(err.to_string()));
                Ok(Response::internal_error(req.id()).body(err).to_vec()?)
            }
        }
    }

    pub async fn create_forwarder(
        &self,
        ctx: &Context,
        req: CreateForwarder,
    ) -> Result<ForwarderInfo> {
        debug!(addr = %req.address(), alias = ?req.alias(), "Handling CreateForwarder request");

        let manager = self.node_manager.clone();

        let connection = Connection::new(ctx, req.address())
            .with_authorized_identity(req.authorized())
            .add_default_consumers();

        let connection_instance =
            NodeManager::connect(self.node_manager.clone(), connection).await?;

        // Add all Hop workers as consumers for Demo purposes
        // Production nodes should not run any Hop workers
        for hop in manager.read().await.registry.hop_services.keys() {
            connection_instance.add_consumer(ctx, hop);
        }

        let options = RemoteForwarderOptions::new();

        let route = local_multiaddr_to_route(&connection_instance.normalized_addr)
            .ok_or_else(|| ApiError::core("invalid address: {addr}"))?;

        let forwarder = if req.at_rust_node() {
            if let Some(alias) = req.alias() {
                RemoteForwarder::create_static_without_heartbeats(ctx, route, alias, options).await
            } else {
                RemoteForwarder::create(ctx, route, options).await
            }
        } else {
            let result = if let Some(alias) = req.alias() {
                RemoteForwarder::create_static(ctx, route, alias, options).await
            } else {
                RemoteForwarder::create(ctx, route, options).await
            };
            if result.is_ok() && !connection_instance.transport_route.is_empty() {
                let ping_route = connection_instance.transport_route.clone();
                let ctx = Arc::new(ctx.async_try_clone().await?);
                let repl = replacer(
                    manager,
                    ctx,
                    connection_instance,
                    req.address().clone(),
                    req.alias().map(|a| a.to_string()),
                    req.authorized(),
                );
                let node_manager = self.node_manager.write().await;
                let mut session = Session::new(ping_route);
                session.set_replacer(repl);
                node_manager.add_session(session);
            }
            result
        };

        match forwarder {
            Ok(info) => {
                let registry_info = info.clone();
                let registry_remote_address = registry_info.remote_address().to_string();
                let forwarder_info = ForwarderInfo::from(info);
                let mut node_manager = self.node_manager.write().await;
                node_manager
                    .registry
                    .forwarders
                    .insert(registry_remote_address, registry_info);

                debug!(
                    forwarding_route = %forwarder_info.forwarding_route(),
                    remote_address = %forwarder_info.remote_address_ma()?,
                    "CreateForwarder request processed, sending back response"
                );
                Ok(forwarder_info)
            }
            Err(err) => {
                error!(?err, "Failed to create forwarder");
                Err(err)
            }
        }
    }

    pub(super) async fn delete_forwarder(
        &mut self,
        ctx: &mut Context,
        req: &Request,
        remote_address: &str,
    ) -> Result<ResponseBuilder<Option<ForwarderInfo>>, ResponseBuilder<Error>> {
        let mut node_manager = self.node_manager.write().await;

        debug!(%remote_address , "Handling DeleteForwarder request");

        if let Some(forwarder_to_delete) = node_manager.registry.forwarders.remove(remote_address) {
            debug!(%remote_address, "Successfully removed forwarder from node registry");

            match ctx
                .stop_worker(forwarder_to_delete.worker_address().clone())
                .await
            {
                Ok(_) => {
                    debug!(%remote_address, "Successfully stopped forwarder");
                    Ok(Response::ok(req.id())
                        .body(Some(ForwarderInfo::from(forwarder_to_delete.to_owned()))))
                }
                Err(err) => {
                    error!(%remote_address, ?err, "Failed to delete forwarder from node registry");
                    let err_body = Error::new(req.path())
                        .with_message(format!("Failed to delete forwarder at {}.", remote_address))
                        .with_cause(Error::new(req.path()).with_message(err.to_string()));
                    Err(Response::internal_error(req.id()).body(err_body))
                }
            }
        } else {
            error!(%remote_address, "Forwarder not found in the node registry");
            let err_body = Error::new(req.path()).with_message(format!(
                "Forwarder with address {} not found.",
                remote_address
            ));
            Err(Response::not_found(req.id()).body(err_body))
        }
    }

    pub(super) async fn show_forwarder(
        &mut self,
        req: &Request,
        remote_address: &str,
    ) -> Result<ResponseBuilder<Option<ForwarderInfo>>, ResponseBuilder<Error>> {
        debug!("Handling ShowForwarder request");
        let node_manager = self.node_manager.read().await;
        if let Some(forwarder_to_show) = node_manager.registry.forwarders.get(remote_address) {
            debug!(%remote_address, "Forwarder not found in node registry");
            Ok(
                Response::ok(req.id())
                    .body(Some(ForwarderInfo::from(forwarder_to_show.to_owned()))),
            )
        } else {
            error!(%remote_address, "Forwarder not found in the node registry");
            let err_body = Error::new(req.path()).with_message(format!(
                "Forwarder with address {} not found.",
                remote_address
            ));
            Err(Response::not_found(req.id()).body(err_body))
        }
    }

    pub async fn get_forwarders(&self) -> Vec<ForwarderInfo> {
        let registry = &self.node_manager.read().await.registry.forwarders;
        let forwarders = registry
            .iter()
            .map(|(_, registry_info)| ForwarderInfo::from(registry_info.to_owned()))
            .collect();
        trace!(?forwarders, "Forwarders retrieved");
        forwarders
    }

    pub(super) async fn get_forwarders_response(
        &self,
        req: &Request,
    ) -> ResponseBuilder<Vec<ForwarderInfo>> {
        debug!("Handling ListForwarders request");
        Response::ok(req.id()).body(self.get_forwarders().await)
    }
}

/// Create a session replacer.
///
/// This returns a function that accepts the previous ping address (e.g.
/// the secure channel worker address) and constructs the whole route
/// again.
fn replacer(
    manager: Arc<RwLock<NodeManager>>,
    ctx: Arc<Context>,
    connection_instance: ConnectionInstance,
    addr: MultiAddr,
    alias: Option<String>,
    auth: Option<Identifier>,
) -> Replacer {
    let connection_instance_arc = Arc::new(Mutex::new(connection_instance));
    Box::new(move |prev_route| {
        let ctx = ctx.clone();
        let addr = addr.clone();
        let alias = alias.clone();
        let auth = auth.clone();
        let node_manager_arc = manager.clone();
        let connection_instance_arc = connection_instance_arc.clone();
        let previous_connection_instance = connection_instance_arc.lock().unwrap().clone();

        Box::pin(async move {
            debug!(%prev_route, %addr, "creating new remote forwarder");

            let f = async {
                let mut node_manager = node_manager_arc.write().await;
                for encryptor in &previous_connection_instance.secure_channel_encryptors {
                    if let Err(error) = node_manager.delete_secure_channel(&ctx, encryptor).await {
                        //not much we can do about it
                        debug!("cannot delete secure channel `{encryptor}`: {error}");
                    }
                }
                if let Some(tcp_connection) = previous_connection_instance.tcp_connection.as_ref() {
                    if let Err(error) = node_manager
                        .tcp_transport
                        .disconnect(tcp_connection.sender_address().clone())
                        .await
                    {
                        debug!("cannot stop tcp worker `{tcp_connection}`: {error}");
                    }
                }
                drop(node_manager);

                let connection = Connection::new(ctx.as_ref(), &addr)
                    .with_authorized_identity(auth)
                    .with_timeout(MAX_CONNECT_TIME)
                    .add_default_consumers();

                let new_connection_instance =
                    NodeManager::connect(node_manager_arc, connection).await?;

                *connection_instance_arc.lock().unwrap() = new_connection_instance.clone();

                let route = local_multiaddr_to_route(&new_connection_instance.normalized_addr)
                    .ok_or_else(|| {
                        ApiError::core(format!(
                            "invalid multiaddr: {}",
                            &new_connection_instance.normalized_addr
                        ))
                    })?;

                let options = RemoteForwarderOptions::new();
                if let Some(alias) = &alias {
                    RemoteForwarder::create_static(&ctx, route, alias, options).await?;
                } else {
                    RemoteForwarder::create(&ctx, route, options).await?;
                }

                Ok(new_connection_instance.transport_route)
            };
            match timeout(MAX_RECOVERY_TIME, f).await {
                Err(_) => {
                    warn!(%addr, "timeout creating new remote forwarder");
                    Err(ApiError::core("timeout"))
                }
                Ok(Err(e)) => {
                    warn!(%addr, err = %e, "error creating new remote forwarder");
                    Err(e)
                }
                Ok(Ok(a)) => Ok(a),
            }
        })
    })
}
