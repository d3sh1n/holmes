/// Why a finding cannot be filed against an authorized program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingGateError {
    NoVerifiedResolution,
    UnknownResolution(String),
    UnverifiedResolution { id: String, status: String },
    OutOfScopeAsset(String),
    MissingAffectedAsset,
    NoActiveProgram,
}

impl std::fmt::Display for FindingGateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoVerifiedResolution => write!(
                f,
                "bounty workflow: findings must cite a verified Ledger Resolution ID"
            ),
            Self::UnknownResolution(id) => {
                write!(f, "bounty workflow: unknown Resolution ID {id}")
            }
            Self::UnverifiedResolution { id, status } => write!(
                f,
                "bounty workflow: Resolution {id} is {status}, not verified (confirmed/rejected)"
            ),
            Self::OutOfScopeAsset(asset) => write!(
                f,
                "bounty workflow: asset '{asset}' is out of the active program scope"
            ),
            Self::MissingAffectedAsset => write!(
                f,
                "bounty workflow: findings must name an in-scope affected_asset or location"
            ),
            Self::NoActiveProgram => {
                write!(f, "bounty workflow: no authorized program scope is active")
            }
        }
    }
}

/// Gate used by PreGuard / Runtime: a finding against an active program must
/// cite at least one verified resolution and an in-scope asset.
pub fn gate_finding(
    program: Option<&ProgramScope>,
    resolution_ids: &[String],
    resolution_status: impl Fn(&str) -> Option<String>,
    affected_asset: Option<&str>,
) -> Result<(), FindingGateError> {
    let Some(program) = program else {
        return Ok(());
    };
    if resolution_ids.is_empty() {
        return Err(FindingGateError::NoVerifiedResolution);
    }
    for id in resolution_ids {
        match resolution_status(id) {
            None => return Err(FindingGateError::UnknownResolution(id.clone())),
            Some(status) => {
                let ok = matches!(status.as_str(), "confirmed" | "rejected");
                if !ok {
                    return Err(FindingGateError::UnverifiedResolution {
                        id: id.clone(),
                        status,
                    });
                }
            }
        }
    }
    let asset = affected_asset.map(str::trim).unwrap_or("");
    if asset.is_empty() {
        return Err(FindingGateError::MissingAffectedAsset);
    }
    if !identifier_in_scope(program, asset) {
        return Err(FindingGateError::OutOfScopeAsset(asset.to_string()));
    }
    Ok(())
}

pub fn record_asset_or_reject(
    program: Option<&ProgramScope>,
    identifier: &str,
) -> Result<(), String> {
    let Some(program) = program else {
        return Err("no authorized program scope is active; call set_program_scope first".into());
    };
    if identifier.trim().is_empty() {
        return Err("asset identifier is required".into());
    }
    if !identifier_in_scope(program, identifier) {
        return Err(format!(
            "refusing to record out-of-scope asset '{identifier}'"
        ));
    }
    Ok(())
}

/// Does `identifier` (host, URL, CIDR, or suffix) fall inside the program?
pub fn identifier_in_scope(program: &ProgramScope, identifier: &str) -> bool {
    let trimmed = identifier.trim();
    // A path-only location does not name an external host; it is in-scope as long
    // as a program is active. Full URLs and hosts are still checked.
    if trimmed.starts_with('/') && !trimmed.contains("://") {
        return true;
    }
    match classify_identifier(identifier) {
        IdentifierClass::Url(url) => url_in_program_scope(program, &url),
        IdentifierClass::HostOrCidr(host) => host_in_program_scope(program, &host),
    }
}

enum IdentifierClass {
    Url(String),
    HostOrCidr(String),
}

fn classify_identifier(raw: &str) -> IdentifierClass {
    let trimmed = raw.trim();
    if trimmed.contains("://") || trimmed.starts_with('/') {
        IdentifierClass::Url(trimmed.to_string())
    } else {
        IdentifierClass::HostOrCidr(trimmed.to_string())
    }
}

pub fn host_in_program_scope(program: &ProgramScope, host: &str) -> bool {
    host_verdict(
        host,
        &program.allow_hosts(),
        &program.deny_hosts(),
        program.allow_private,
    )
    .is_ok()
}

pub fn url_in_program_scope(program: &ProgramScope, url: &str) -> bool {
    if url_matches_any_prefix(url, &program.deny_url_prefixes()) {
        return false;
    }
    if let Some(host) = host_of_url(url) {
        if host_verdict(
            &host,
            &program.allow_hosts(),
            &program.deny_hosts(),
            program.allow_private,
        )
        .is_err()
        {
            return false;
        }
    }
    let prefixes = program.url_prefixes();
    if prefixes.is_empty() {
        return host_of_url(url)
            .map(|h| host_in_program_scope(program, &h))
            .unwrap_or(false);
    }
    url_matches_any_prefix(url, &prefixes)
}

/// Host allow/deny matching used by ScopeGuard (exact host, domain suffix, IPv4 CIDR).
pub fn host_verdict(
    host: &str,
    allow: &[String],
    deny: &[String],
    allow_private: bool,
) -> Result<(), String> {
    let host = host.trim().trim_end_matches('.').to_lowercase();
    if host.is_empty() {
        return Ok(());
    }
    if deny.iter().any(|d| matches_entry(&host, d)) {
        return Err(format!(
            "host '{host}' is explicitly out of scope (deny list)"
        ));
    }
    if is_private_or_metadata(&host) && !allow_private {
        return Err(format!(
            "host '{host}' is a private/loopback/link-local/metadata address; blocked \
             (set allow_private to permit)"
        ));
    }
    if allow.is_empty() {
        return Err(format!(
            "host '{host}' is not in the engagement scope allowlist — only assigned \
             targets may be touched"
        ));
    }
    if !allow.iter().any(|a| matches_entry(&host, a)) {
        return Err(format!(
            "host '{host}' is not in the engagement scope allowlist — only assigned \
             targets may be touched"
        ));
    }
    Ok(())
}

/// Does `host` match an allow/deny entry? Supports exact host, domain-suffix
/// (`example.com` matches `api.example.com`), bare IP, and IPv4 CIDR (`10.0.0.0/8`).
pub fn matches_entry(host: &str, entry: &str) -> bool {
    let entry = normalize_entry(entry);
    if entry.is_empty() {
        return false;
    }
    if host == entry {
        return true;
    }
    if !entry.contains('/') && host.ends_with(&format!(".{entry}")) {
        return true;
    }
    if let Some((base, bits)) = parse_cidr(&entry) {
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            let ip_u = u32::from(ip);
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            return (ip_u & mask) == (base & mask);
        }
    }
    false
}

pub fn parse_cidr(entry: &str) -> Option<(u32, u8)> {
    let (addr, bits) = entry.split_once('/')?;
    let ip: Ipv4Addr = addr.parse().ok()?;
    let bits: u8 = bits.parse().ok()?;
    if bits > 32 {
        return None;
    }
    Some((u32::from(ip), bits))
}

pub fn host_of_url(url: &str) -> Option<String> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = if host.starts_with('[') {
        host.trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}
