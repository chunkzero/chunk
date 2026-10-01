use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    #[default]
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

impl HttpMethod {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpRequest {
    pub url: String,
    #[serde(default)]
    pub method: HttpMethod,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
}

/// Receiving a complete HTTP response does not imply application success.
/// Unknown means dispatch began and the remote effect may have completed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum HttpOutcome {
    /// `url` is where the response came from, after any redirects.
    Completed {
        effect_id: String,
        url: String,
        status: u16,
        headers: BTreeMap<String, String>,
        body: String,
    },
    Rejected {
        effect_id: String,
        reason: String,
    },
    Unknown {
        effect_id: String,
        reason: String,
    },
}
