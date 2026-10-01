use super::*;
use crate::{DeploymentId, Engine, Error, Limits};

struct Host;
impl ActionHost for Host {
    fn call(&self, _: u32, _: Mode, _: String, _: Json) -> Pin<Box<dyn Future<Output = Result<String, String>>>> {
        Box::pin(async { Err("unexpected transaction".into()) })
    }
}

#[test]
fn action_deadline_expires_a_sleep_without_retaining_its_runtime() {
    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("deadline").unwrap();
    engine
        .register(
            id.clone(),
            "export async function work(ctx) { await ctx.sleep(1000); return 1; }".into(),
            Limits::default(),
        )
        .unwrap();
    let result = engine.execute_action(
        &id,
        ActionInvocation {
            id: "one".into(),
            export: "work".into(),
            arguments: serde_json::Value::Null.into(),
            caller: serde_json::Value::Null.into(),
            timestamp: 0,
            seed: 0,
            deadline: Instant::now() + Duration::from_millis(50),
            env: Json::empty(),
        },
        Rc::new(Host),
        &Cancellation::default(),
    );
    assert!(matches!(result, Err(Error::Deadline)), "{result:?}");
    assert!(engine.release(&id));
}
