use url::Url;

/// Normalize repo URL to canonical HTTPS format
/// Accepts: HTTPS, HTTP, SSH (scp-like user@host:path or ssh://), git:// URLs
/// Returns: Canonical HTTPS URL without credentials, default ports, `.git`
/// suffix, or trailing slash. Non-default ports are part of repository
/// identity and are preserved (for example Gerrit's SSH port 29418).
pub fn normalize_repo_url(url_str: &str) -> Result<String, String> {
    let url_str = url_str.trim();

    // Handle SSH scp-like format: user@host:path
    if !url_str.contains("://")
        && let Some((user_host, path)) = url_str.split_once(':')
        && let Some((_, host)) = user_host.rsplit_once('@')
    {
        return normalize_ssh_url(host, path);
    }

    // Older clients and server-facing records may already use the canonical
    // scheme-less `host[:port]/path` form. Treat it as HTTPS input.
    let parse_input = if url_str.contains("://") {
        url_str.to_string()
    } else {
        format!("https://{url_str}")
    };
    let url = Url::parse(&parse_input).map_err(|e| format!("Invalid URL: {}", e))?;

    // Validate scheme
    let scheme = url.scheme();
    if !["https", "http", "git", "ssh"].contains(&scheme) {
        return Err(format!("Unsupported URL scheme: {}", scheme));
    }

    // Extract host
    let host = url.host_str().ok_or("URL must have a host")?;
    let host = format_url_host(host);
    let authority = match url.port() {
        Some(port) if !is_default_port(scheme, port) => format!("{host}:{port}"),
        _ => host,
    };

    let path = normalize_repo_path(url.path());

    // Build canonical HTTPS URL
    let canonical = format!("https://{authority}/{path}");

    // Validate the normalized URL
    validate_normalized_url(&canonical)?;

    Ok(canonical)
}

/// Validate that normalized URL is a proper HTTPS URL
fn validate_normalized_url(url_str: &str) -> Result<(), String> {
    let url = Url::parse(url_str).map_err(|e| format!("Failed to parse normalized URL: {}", e))?;

    if url.scheme() != "https" {
        return Err("Normalized URL must be HTTPS".to_string());
    }

    if url.host_str().is_none() {
        return Err("Normalized URL must have a valid host".to_string());
    }

    // Ensure path is not empty (at minimum /)
    if url.path().is_empty() || url.path() == "/" {
        return Err("Normalized URL must have a path (repo identifier)".to_string());
    }

    Ok(())
}

/// Normalize SSH scp-like URL (user@host:path) to HTTPS
fn normalize_ssh_url(host: &str, path: &str) -> Result<String, String> {
    if host.is_empty() || path.is_empty() {
        return Err("Invalid SSH URL format".to_string());
    }

    // Normalize path
    let host = format_url_host(host);
    let path = normalize_repo_path(path);

    let canonical = format!("https://{}/{}", host, path);

    // Validate the normalized URL
    validate_normalized_url(&canonical)?;

    Ok(canonical)
}

fn is_default_port(scheme: &str, port: u16) -> bool {
    matches!(
        (scheme, port),
        ("https", 443) | ("http", 80) | ("ssh", 22) | ("git", 9418)
    )
}

fn format_url_host(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    }
}

fn strip_git_suffix(mut path: &str) -> &str {
    while path
        .len()
        .checked_sub(4)
        .and_then(|start| path.get(start..))
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".git"))
    {
        path = &path[..path.len() - 4];
    }
    path
}

fn normalize_repo_path(path: &str) -> String {
    let path = path.trim_matches('/');
    let path = strip_git_suffix(path);
    // Gerrit commonly exposes the authenticated HTTP clone route below /a/.
    // It is a transport marker, not part of repository identity.
    path.strip_prefix("a/").unwrap_or(path).to_string()
}

/// Resolve a normalized repo URL from an already-opened Repository.
///
/// Finds the default remote and normalizes its URL to canonical HTTPS format.
/// Returns None if there is no remote or the URL cannot be normalized.
pub fn resolve_repo_url_from_repo(repo: &crate::git::repository::Repository) -> Option<String> {
    let remote_name = repo.get_default_remote().ok()??;
    let remotes = repo.remotes_with_urls().ok()?;
    let (_, url) = remotes.into_iter().find(|(n, _)| n == &remote_name)?;
    normalize_repo_url(&url).ok()
}

/// Resolve a normalized repo URL from a filesystem path.
///
/// Opens the git repository at `work_dir` (or discovers it by walking up),
/// finds the default remote, and normalizes its URL to canonical HTTPS format.
/// Returns None if the path is not in a git repo, has no remote, or the URL
/// cannot be normalized.
pub fn resolve_repo_url_from_path(work_dir: &std::path::Path) -> Option<String> {
    let repo = crate::git::repository::discover_repository_in_path_no_git_exec(work_dir).ok()?;
    resolve_repo_url_from_repo(&repo)
}

#[cfg(test)]
mod tests {
    use super::normalize_repo_url;

    #[test]
    fn test_normalize_repo_url_https() {
        assert_eq!(
            normalize_repo_url("https://github.com/user/repo").unwrap(),
            "https://github.com/user/repo"
        );
        assert_eq!(
            normalize_repo_url("https://github.com/user/repo.git").unwrap(),
            "https://github.com/user/repo"
        );
        assert_eq!(
            normalize_repo_url("https://github.com/user/repo/").unwrap(),
            "https://github.com/user/repo"
        );
        assert_eq!(
            normalize_repo_url("https://gitlab.com/group/subgroup/repo.git/").unwrap(),
            "https://gitlab.com/group/subgroup/repo"
        );
        assert_eq!(
            normalize_repo_url("github.com/user/repo.git").unwrap(),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_ssh() {
        assert_eq!(
            normalize_repo_url("git@github.com:user/repo.git").unwrap(),
            "https://github.com/user/repo"
        );
        assert_eq!(
            normalize_repo_url("ssh://git@github.com/user/repo.git").unwrap(),
            "https://github.com/user/repo"
        );
        assert_eq!(
            normalize_repo_url("alice@github.com:org/repo").unwrap(),
            "https://github.com/org/repo"
        );
        assert_eq!(
            normalize_repo_url("git@gitlab.com:group/subgroup/repo").unwrap(),
            "https://gitlab.com/group/subgroup/repo"
        );
        assert_eq!(
            normalize_repo_url("git@bitbucket.org:user/repo.git").unwrap(),
            "https://bitbucket.org/user/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_git_protocol() {
        assert_eq!(
            normalize_repo_url("git://github.com/user/repo.git").unwrap(),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_http_upgrade() {
        assert_eq!(
            normalize_repo_url("http://github.com/user/repo").unwrap(),
            "https://github.com/user/repo"
        );
        assert_eq!(
            normalize_repo_url("https://token@github.com/user/repo").unwrap(),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_invalid() {
        assert!(normalize_repo_url("not-a-url").is_err());
        assert!(normalize_repo_url("https://").is_err());
        assert!(normalize_repo_url("ftp://example.com/repo").is_err());
        assert!(normalize_repo_url("git@github.com").is_err()); // missing :path
    }

    #[test]
    fn test_normalize_repo_url_ssh_scp_edge_cases() {
        // SSH URL with leading slash in path
        assert_eq!(
            normalize_repo_url("git@github.com:/user/repo").unwrap(),
            "https://github.com/user/repo"
        );

        // SSH URL with multiple path segments
        assert_eq!(
            normalize_repo_url("git@gitlab.example.com:group/subgroup/nested/repo").unwrap(),
            "https://gitlab.example.com/group/subgroup/nested/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_empty_or_invalid_ssh() {
        // Missing path after colon
        let result = normalize_repo_url("git@github.com:");
        assert!(result.is_err());

        // Empty string
        let result = normalize_repo_url("");
        assert!(result.is_err());

        // Only whitespace
        let result = normalize_repo_url("   ");
        assert!(result.is_err());
    }

    #[test]
    fn test_normalize_repo_url_with_credentials() {
        // HTTPS with user credentials should strip them
        assert_eq!(
            normalize_repo_url("https://user:pass@github.com/user/repo").unwrap(),
            "https://github.com/user/repo"
        );

        // HTTPS with token
        assert_eq!(
            normalize_repo_url("https://oauth2:token123@gitlab.com/user/repo").unwrap(),
            "https://gitlab.com/user/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_with_port() {
        // Protocol-default ports are omitted.
        assert_eq!(
            normalize_repo_url("https://github.com:443/user/repo").unwrap(),
            "https://github.com/user/repo"
        );

        // SSH URL with port
        assert_eq!(
            normalize_repo_url("ssh://git@github.com:22/user/repo.git").unwrap(),
            "https://github.com/user/repo"
        );

        // Non-default ports are repository identity, notably Gerrit SSH.
        assert_eq!(
            normalize_repo_url("ssh://git@review.example.com:29418/a/team/repo.git").unwrap(),
            "https://review.example.com:29418/team/repo"
        );
        assert_eq!(
            normalize_repo_url("https://review.example.com:8443/a/team/repo.git").unwrap(),
            "https://review.example.com:8443/team/repo"
        );
        assert_ne!(
            normalize_repo_url("https://review.example.com:8443/a/team/repo.git").unwrap(),
            normalize_repo_url("https://review.example.com:9443/a/team/repo.git").unwrap()
        );
        assert_eq!(
            normalize_repo_url("git@github.com:Org/Repo.git").unwrap(),
            normalize_repo_url("https://github.com/Org/Repo").unwrap()
        );
        assert_eq!(
            normalize_repo_url("ssh://git@review.example.com:29418/a/team/repo.git").unwrap(),
            normalize_repo_url("https://review.example.com:29418/team/repo").unwrap()
        );
    }

    #[test]
    fn test_normalize_repo_url_no_path() {
        // URL with no path (just host)
        let result = normalize_repo_url("https://github.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("path"));

        // URL with only slash
        let result = normalize_repo_url("https://github.com/");
        assert!(result.is_err());
    }

    #[test]
    fn test_normalize_repo_url_complex_paths() {
        // Multiple .git suffixes (strips all at the end)
        assert_eq!(
            normalize_repo_url("https://github.com/user/repo.git.git").unwrap(),
            "https://github.com/user/repo"
        );

        // Path with underscores and dashes
        assert_eq!(
            normalize_repo_url("https://github.com/my-org/my_repo-123").unwrap(),
            "https://github.com/my-org/my_repo-123"
        );

        // Path with dots (not .git)
        assert_eq!(
            normalize_repo_url("https://github.com/user/repo.v2").unwrap(),
            "https://github.com/user/repo.v2"
        );

        // Nested paths
        assert_eq!(
            normalize_repo_url("https://gitlab.com/group/subgroup/project.git").unwrap(),
            "https://gitlab.com/group/subgroup/project"
        );

        assert_eq!(
            normalize_repo_url("HTTPS://GitHub.COM/Org/Repo.GIT").unwrap(),
            "https://github.com/Org/Repo"
        );
    }

    #[test]
    fn test_validate_normalized_url() {
        use super::validate_normalized_url;

        // Valid HTTPS URL with path
        assert!(validate_normalized_url("https://github.com/user/repo").is_ok());

        // Missing HTTPS scheme
        assert!(validate_normalized_url("http://github.com/user/repo").is_err());

        // No path
        assert!(validate_normalized_url("https://github.com").is_err());
        assert!(validate_normalized_url("https://github.com/").is_err());
    }

    #[test]
    fn test_normalize_ssh_url_edge_cases() {
        use super::normalize_ssh_url;

        // Valid SSH path with trailing slash
        assert_eq!(
            normalize_ssh_url("github.com", "user/repo/").unwrap(),
            "https://github.com/user/repo"
        );

        // Empty host
        assert!(normalize_ssh_url("", "user/repo").is_err());

        // Empty path
        assert!(normalize_ssh_url("github.com", "").is_err());

        // Path with .git suffix
        assert_eq!(
            normalize_ssh_url("gitlab.com", "group/repo.git").unwrap(),
            "https://gitlab.com/group/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_whitespace_handling() {
        // Leading/trailing whitespace
        assert_eq!(
            normalize_repo_url("  https://github.com/user/repo  ").unwrap(),
            "https://github.com/user/repo"
        );

        // Whitespace around SSH URL
        assert_eq!(
            normalize_repo_url("  git@github.com:user/repo.git  ").unwrap(),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn test_normalize_repo_url_unsupported_schemes() {
        assert!(normalize_repo_url("ftp://example.com/repo").is_err());
        assert!(normalize_repo_url("file:///local/path").is_err());
        assert!(normalize_repo_url("svn://example.com/repo").is_err());
    }
}
