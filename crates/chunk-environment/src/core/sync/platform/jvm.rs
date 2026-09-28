//! A JVM's registration, reports and method results, `chunk:register`, `chunk:report` and `chunk:method_result`, which
//! control applies. The JVM's host comes from its credential.

use super::{
    super::{
        SyncService, app,
        auth::{Class, Principal},
        errors, position,
    },
    decode,
};
use chunk_js::DeploymentId;
use chunk_proto::sync::v1::{
    CallRequest, Error, JvmMethodResult, JvmRegistered, JvmRegistration, JvmReport, Position, error::Code,
};
use chunk_store::Revision;
use prost::Message;
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Copy)]
pub(super) enum Method {
    Register,
    Report,
    Result,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "register" => Self::Register,
            "report" => Self::Report,
            "method_result" => Self::Result,
            _ => return None,
        })
    }
}

/// Runs `method` for the JVM `principal` names, returning its encoded result and control's position after a report.
pub(super) async fn call(
    service: &SyncService,
    principal: &Principal,
    method: Method,
    request: &CallRequest,
) -> Result<(Option<Position>, Vec<u8>), Error> {
    let (Class::Jvm { host } | Class::Unadopted { host }) = &principal.class else {
        return Err(errors::denied("only a JVM registers and reports"));
    };
    let host = host.clone();
    if !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("a JVM method takes no deployment or caller"));
    }
    match method {
        Method::Register => {
            if !request.stream.is_empty() {
                return Err(errors::invalid("a registration names no stream"));
            }
            let registration: JvmRegistration = decode(&request.arguments)?;
            check_endpoint(&registration.player_endpoint, principal.peer, service.private_address)?;
            let unknown = |_| errors::error(Code::Contract, "unknown deployment");
            let deployment = DeploymentId::new(&registration.deployment).map_err(unknown)?;
            if let Err(failure) = service.app.backend().check_deployment(deployment).await {
                // A JVM that outlived core but whose deployment a restore lost can never register, so control adopts
                // it by its launch record only to stop it.
                if matches!(principal.class, Class::Unadopted { .. })
                    && matches!(failure, chunk_backend::Error::Unknown)
                {
                    let stopped = service.control.stop_survivor(&host, &principal.credential, &registration);
                    stopped.map_err(|failure| errors::operation(&failure))?;
                }
                return Err(errors::backend(&failure));
            }
            let registered = service.control.register_jvm(&host, &principal.credential, registration);
            registered.map_err(|failure| errors::operation(&failure))?;
            Ok((None, JvmRegistered { host }.encode_to_vec()))
        }
        Method::Report => {
            let report: JvmReport = decode(&request.arguments)?;
            // Not an accepted operation: shutdown must not wait on a JVM that keeps reporting before it stops it.
            let reported = service.control.report_jvm(&host, &principal.credential, &request.stream, report);
            reported.map_err(|failure| errors::operation(&failure))?;
            let generation = *service.control.subscribe().borrow();
            Ok((position(generation.epoch, Revision(generation.revision)), Vec::new()))
        }
        Method::Result => {
            if request.operation_id.is_empty() {
                return Err(errors::invalid("a method result names the method's operation ID"));
            }
            app::reject_reserved(&request.operation_id)?;
            let result: JvmMethodResult = decode(&request.arguments)?;
            let recorded = service.control.method_result(&host, &request.stream, &request.operation_id, result);
            recorded.map_err(|failure| errors::operation(&failure))?;
            Ok((None, Vec::new()))
        }
    }
}

/// Requires a player endpoint with a port that gateways may dial: a JVM connecting from another machine names the
/// address it connects from, and one connecting over loopback runs on core's machine, so it names loopback or that
/// machine's `private_address`.
fn check_endpoint(endpoint: &str, peer: Option<SocketAddr>, private_address: Option<IpAddr>) -> Result<(), Error> {
    let address: SocketAddr = endpoint.parse().map_err(|_| errors::invalid("the player endpoint is not an address"))?;
    let named = address.ip().to_canonical();
    let allowed = match peer.map(|peer| peer.ip().to_canonical()) {
        Some(peer) if peer.is_loopback() => {
            named.is_loopback() || private_address.is_some_and(|private| private.to_canonical() == named)
        }
        Some(peer) => named == peer,
        None => false,
    };
    if !allowed || address.port() == 0 || !chunk_service::net::private(named) {
        return Err(errors::denied("the player endpoint must be on the JVM's own machine"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jvms_name_their_peer_or_their_machines_addresses() {
        let private = Some("fdaa::2".parse().unwrap());
        let loopback = Some("127.0.0.1:4000".parse().unwrap());
        let remote = Some("[::ffff:10.0.0.3]:4000".parse().unwrap());
        let remote_v6 = Some("[fdaa::3]:4000".parse().unwrap());
        let denied = Some(Code::Denied);
        for (endpoint, peer, expected) in [
            ("127.0.0.1:25565", loopback, None),
            ("[::1]:25565", loopback, None),
            ("[fdaa::2]:25565", loopback, None),
            ("[fdaa::3]:25565", loopback, denied),
            ("10.0.0.3:25565", loopback, denied),
            ("127.0.0.1:0", loopback, denied),
            ("10.0.0.3:25565", remote, None),
            ("[fdaa::3]:25565", remote_v6, None),
            ("[fdaa::4]:25565", remote_v6, denied),
            ("10.0.0.4:25565", remote, denied),
            ("127.0.0.1:25565", remote, denied),
            ("[fdaa::2]:25565", remote, denied),
            ("127.0.0.1:25565", None, denied),
            ("http://127.0.0.1:25565", loopback, Some(Code::Invalid)),
        ] {
            let code = check_endpoint(endpoint, peer, private).err().map(|error| error.code());
            assert_eq!(code, expected, "{endpoint} from {peer:?}");
        }
    }
}
