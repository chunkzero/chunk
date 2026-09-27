//! A JVM's registration and reports, `chunk:register` and `chunk:report`, which control applies like the
//! supervisor's. The JVM's host comes from its credential.

use super::{
    super::{
        SyncService,
        auth::{Class, Principal},
        errors, position,
    },
    run,
};
use chunk_js::DeploymentId;
use chunk_proto::sync::v1::{CallRequest, Error, JvmRegistered, JvmRegistration, JvmReport, Position, error::Code};
use chunk_store::Revision;
use prost::Message;

#[derive(Clone, Copy)]
pub(super) enum Method {
    Register,
    Report,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "register" => Self::Register,
            "report" => Self::Report,
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
    let host = match (&principal.class, method) {
        (Class::Jvm { host }, _) | (Class::Unadopted { host }, Method::Register) => host.clone(),
        (Class::Unadopted { .. }, Method::Report) => return Err(errors::denied("the JVM must register again first")),
        _ => return Err(errors::denied("only a JVM registers and reports")),
    };
    if !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("a JVM method takes no deployment or caller"));
    }
    match method {
        Method::Register => {
            if !request.stream.is_empty() {
                return Err(errors::invalid("a registration names no stream"));
            }
            let registration: JvmRegistration = decode(&request.arguments)?;
            let unknown = |_| errors::error(Code::Contract, "unknown deployment");
            let deployment = DeploymentId::new(&registration.deployment).map_err(unknown)?;
            let backend = service.app.backend();
            backend.check_deployment(deployment).await.map_err(|failure| errors::backend(&failure))?;
            let registered = service.control.register_jvm(&host, &principal.credential, registration);
            registered.map_err(|failure| errors::operation(&failure))?;
            Ok((None, JvmRegistered { host }.encode_to_vec()))
        }
        Method::Report => {
            let report: JvmReport = decode(&request.arguments)?;
            let (control, credential, stream) =
                (service.control.clone(), principal.credential.clone(), request.stream.clone());
            let reported = run(service, async move { control.report_jvm(&host, &credential, &stream, report).await });
            reported.await.map_err(|failure| errors::operation(&failure))?;
            let generation = *service.control.subscribe().borrow();
            Ok((position(generation.epoch, Revision(generation.revision)), Vec::new()))
        }
    }
}

fn decode<T: Message + Default>(arguments: &[u8]) -> Result<T, Error> {
    T::decode(arguments).map_err(|_| errors::invalid("arguments are not the method's message"))
}
