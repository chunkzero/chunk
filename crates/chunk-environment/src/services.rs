use std::{collections::BTreeSet, io, str::FromStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Service {
    /// The environment's store, backend, control and backend functions.
    Core,
    /// The player listener.
    Gateway,
}

/// The services one environment process runs, selected by `CHUNK_SERVICES` as a comma list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Services(BTreeSet<Service>);

impl Services {
    #[must_use]
    pub fn contains(&self, service: Service) -> bool {
        self.0.contains(&service)
    }
}

impl Default for Services {
    fn default() -> Self {
        Self([Service::Core, Service::Gateway].into())
    }
}

impl FromStr for Services {
    type Err = io::Error;

    /// Accepts `core,gateway`, `core` and `gateway`, in any order. A legacy `exec` is ignored with a warning.
    fn from_str(value: &str) -> io::Result<Self> {
        let mut services = BTreeSet::new();
        for name in value.split(',').map(str::trim) {
            match name {
                "core" => _ = services.insert(Service::Core),
                "gateway" => _ = services.insert(Service::Gateway),
                "exec" => tracing::warn!("ignoring exec in CHUNK_SERVICES; core runs backend functions"),
                name => return Err(io::Error::other(format!("unknown service {name:?} in CHUNK_SERVICES"))),
            }
        }
        if services.is_empty() {
            return Err(io::Error::other("CHUNK_SERVICES must be core,gateway, core or gateway"));
        }
        Ok(Self(services))
    }
}
