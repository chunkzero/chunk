//! A JVM's registration, reports and method results, `chunk:register`, `chunk:report` and `chunk:method_result`, which
//! control applies. The JVM's host comes from its credential.

use super::super::{
    SyncService, app,
    auth::{Class, Principal},
    errors, position,
};
use chunk_js::DeploymentId;
use chunk_proto::sync::v1::{
    CallRequest, Error, JvmMethodResult, JvmRegistered, JvmRegistration, JvmReport, Position, error::Code,
};
use chunk_store::Revision;
use prost::Message;

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
            let reported = service.control.report_jvm(&host, &principal.credential, &request.stream, report).await;
            reported.map_err(|failure| errors::operation(&failure))?;
            let generation = *service.control.subscribe().borrow();
            Ok((position(generation.epoch, Revision(generation.revision)), Vec::new()))
        }
        Method::Result => {
            if request.operation_id.is_empty() {
                return Err(errors::invalid("a method result names the method's operation ID"));
            }
            app::reject_prepared(&request.operation_id)?;
            let result: JvmMethodResult = decode(&request.arguments)?;
            let recorded = service.control.method_result(&host, &request.stream, &request.operation_id, result);
            recorded.map_err(|failure| errors::operation(&failure))?;
            Ok((None, Vec::new()))
        }
    }
}

fn decode<T: Message + Default>(arguments: &[u8]) -> Result<T, Error> {
    T::decode(arguments).map_err(|_| errors::invalid("arguments are not the method's message"))
}
