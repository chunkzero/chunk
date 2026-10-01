//! `chunk domains`: an environment's custom hostnames, which route players once their DNS records verify.

use std::io;

use chunk_management::{
    Client,
    v1::{AddDomainRequest, Domain, DomainState, ListDomainsRequest, RemoveDomainRequest, VerifyDomainRequest},
};
use clap::{Args, Subcommand};

use super::{
    EnvironmentArgs, Session, all, api_error, choose,
    resources::{label, table, time},
};

#[derive(Args)]
pub(crate) struct Domains {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Claim a hostname and print the DNS records to create.
    Add {
        /// The hostname players connect to, such as `play.example.com`.
        hostname: String,
        #[command(flatten)]
        environment: EnvironmentArgs,
    },
    /// Check the domain's DNS records now, verifying it when they match.
    Verify {
        /// The domain's hostname or ID.
        domain: String,
        #[command(flatten)]
        environment: EnvironmentArgs,
    },
    /// List the environment's domains.
    List {
        #[command(flatten)]
        environment: EnvironmentArgs,
    },
    /// Remove a domain and its route.
    Remove {
        /// The domain's hostname or ID.
        domain: String,
        #[command(flatten)]
        environment: EnvironmentArgs,
    },
}

pub(super) async fn run(options: Domains) -> io::Result<()> {
    let session = Session::open()?;
    let client = &session.client;
    match options.action {
        Action::Add { hostname, environment } => {
            let (_, environment) = session.environment(&environment).await?;
            let request = AddDomainRequest { environment_id: environment.id, hostname };
            let domain = client.add_domain(&request).await.map_err(api_error)?.domain.unwrap_or_default();
            show(&domain)
        }
        Action::Verify { domain, environment } => {
            let (_, environment) = session.environment(&environment).await?;
            let domain = find(client, &environment.id, &domain).await?;
            let request = VerifyDomainRequest { domain_id: domain.id };
            let domain = client.verify_domain(&request).await.map_err(api_error)?.domain.unwrap_or_default();
            show(&domain)
        }
        Action::List { environment } => {
            let (_, environment) = session.environment(&environment).await?;
            let domains = list(client, &environment.id).await?;
            table(
                ["HOSTNAME", "ID", "STATE", "CREATED"],
                domains.into_iter().map(|domain| {
                    let state = label(domain.state().as_str_name(), "DOMAIN_STATE_");
                    [domain.hostname, domain.id, state, time(domain.create_time)]
                }),
            )
        }
        Action::Remove { domain, environment } => {
            let (_, environment) = session.environment(&environment).await?;
            let domain = find(client, &environment.id, &domain).await?;
            let request = RemoveDomainRequest { domain_id: domain.id };
            client.remove_domain(&request).await.map_err(api_error)?;
            cliclack::log::success(format!("Removed domain {} from {}", domain.hostname, environment.name))
        }
    }
}

/// Prints the domain's state and the DNS records its owner creates.
fn show(domain: &Domain) -> io::Result<()> {
    let hostname = &domain.hostname;
    if domain.state() == DomainState::Verified {
        cliclack::log::success(format!("{hostname} is verified; players can connect to it"))?;
    } else {
        cliclack::log::info(format!(
            "{hostname} is pending verification. Create these DNS records, then run `chunk domains verify {hostname}`."
        ))?;
    }
    table(
        ["TYPE", "NAME", "VALUE"],
        domain.dns_records.iter().map(|record| [record.r#type.clone(), record.name.clone(), record.value.clone()]),
    )
}

/// The environment's domain with this hostname or ID.
async fn find(client: &Client, environment_id: &str, selector: &str) -> io::Result<Domain> {
    let domains = list(client, environment_id).await?;
    choose(domains, Some(selector), "domain", |domain| [&domain.id, &domain.hostname])
}

async fn list(client: &Client, environment_id: &str) -> io::Result<Vec<Domain>> {
    all(|page_token| async move {
        let request = ListDomainsRequest { environment_id: environment_id.into(), page_token, page_size: 0 };
        client.list_domains(&request).await.map(|page| (page.domains, page.next_page_token))
    })
    .await
}
