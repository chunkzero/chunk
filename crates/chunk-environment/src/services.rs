use std::{collections::BTreeSet, io, str::FromStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Service {
    /// The environment's store, backend and control.
    Core,
    /// The player listener.
    Gateway,
    /// Backend code execution.
    Exec,
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
        Self([Service::Core, Service::Gateway, Service::Exec].into())
    }
}

impl FromStr for Services {
    type Err = io::Error;

    /// Accepts `core,gateway,exec` and `core,exec`, in any order.
    fn from_str(value: &str) -> io::Result<Self> {
        let services = value
            .split(',')
            .map(|name| match name.trim() {
                "core" => Ok(Service::Core),
                "gateway" => Ok(Service::Gateway),
                "exec" => Ok(Service::Exec),
                name => Err(io::Error::other(format!("unknown service {name:?} in CHUNK_SERVICES"))),
            })
            .collect::<io::Result<BTreeSet<_>>>()?;
        if !services.contains(&Service::Core) || !services.contains(&Service::Exec) {
            return Err(io::Error::other("CHUNK_SERVICES must be core,gateway,exec or core,exec"));
        }
        Ok(Self(services))
    }
}
