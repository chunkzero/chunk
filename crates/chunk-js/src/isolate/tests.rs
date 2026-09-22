use super::*;

#[test]
fn unwind_exits_the_isolate_before_another_runtime_is_entered() {
    crate::Engine::init_platform();
    let executor = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let deadline = Deadline::new().unwrap();
    let load = || {
        Runtime::load(
            &executor,
            &deadline,
            "chunk:deployment/test",
            "export default () => 42;",
            Limits::default(),
            &Cancellation::default(),
        )
        .unwrap()
    };
    let mut first = load();
    let mut second = load();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| first.with(|_| panic!("unwind")))).is_err());
    second.with(|state| {
        assert!(state.runtime.execute_script("check", "42").is_ok());
    });
    drop(first);
    drop(second);
}
