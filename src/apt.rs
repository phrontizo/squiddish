//! APT-specific optimizations and cache handling

/// Detect if a request is an APT package manager request
pub fn is_apt_request(uri: &str) -> bool {
    // Known APT repository hosts (exact match after scheme)
    let apt_hosts = [
        "://deb.debian.org",
        "://archive.ubuntu.com",
        "://security.ubuntu.com",
        "://security.debian.org",
        "://ppa.launchpad.net",
    ];

    if apt_hosts.iter().any(|host| uri.contains(host)) {
        return true;
    }

    // APT-specific directory structures
    let apt_dirs = [
        "/dists/",
        "/pool/",
    ];

    if apt_dirs.iter().any(|dir| uri.contains(dir)) {
        return true;
    }

    // APT-specific file patterns
    let apt_files = [
        "Packages.gz",
        "Packages.xz",
        "Packages.bz2",
        "Release.gpg",
        "InRelease",
    ];

    if apt_files.iter().any(|pattern| uri.contains(pattern)) {
        return true;
    }

    // .deb files are always APT-related
    uri.ends_with(".deb")
}

/// Determine if an APT request is a package list (should be cached shorter)
/// Package lists change frequently as repositories are updated
pub fn is_apt_package_list(uri: &str) -> bool {
    let list_patterns = [
        "Packages.gz",
        "Packages.xz",
        "Packages.bz2",
        "InRelease",
    ];

    if list_patterns.iter().any(|pattern| uri.contains(pattern)) {
        return true;
    }

    // Release files in /dists/ directories
    uri.contains("/dists/") && (uri.ends_with("/Release") || uri.ends_with("/Release.gpg"))
}

/// Determine if an APT request is a package file (can be cached longer)
/// .deb files are immutable and can be cached for extended periods
pub fn is_apt_package_file(uri: &str) -> bool {
    uri.ends_with(".deb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_apt_request() {
        assert!(is_apt_request("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
        assert!(is_apt_request("http://deb.debian.org/debian/dists/bullseye/Release"));
        assert!(is_apt_request("http://custom-mirror.example.com/dists/focal/main/binary-amd64/Packages.gz"));
        assert!(is_apt_request("http://custom-mirror.example.com/pool/main/a/apache2/apache2.deb"));
        assert!(!is_apt_request("http://example.com/file.tar.gz"));
        // Should NOT match broad patterns like /ubuntu/ or /debian/ in arbitrary URLs
        assert!(!is_apt_request("https://wiki.debian.org/Teams/Dpkg"));
        assert!(!is_apt_request("https://ubuntu.com/download"));
    }

    #[test]
    fn test_is_apt_package_list() {
        assert!(is_apt_package_list("http://archive.ubuntu.com/ubuntu/dists/focal/main/binary-amd64/Packages.gz"));
        assert!(is_apt_package_list("http://archive.ubuntu.com/ubuntu/dists/focal/Release"));
        assert!(is_apt_package_list("http://archive.ubuntu.com/ubuntu/dists/focal/InRelease"));
        assert!(!is_apt_package_list("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
    }

    #[test]
    fn test_is_apt_package_file() {
        assert!(is_apt_package_file("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
        assert!(!is_apt_package_file("http://archive.ubuntu.com/ubuntu/dists/focal/Release"));
    }

    #[test]
    fn test_apt_categorization() {
        let package_url = "http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb";
        let list_url = "http://archive.ubuntu.com/ubuntu/dists/focal/main/binary-amd64/Packages.gz";
        let release_url = "http://archive.ubuntu.com/ubuntu/dists/focal/Release";

        assert!(is_apt_request(package_url));
        assert!(is_apt_package_file(package_url));
        assert!(!is_apt_package_list(package_url));

        assert!(is_apt_request(list_url));
        assert!(is_apt_package_list(list_url));
        assert!(!is_apt_package_file(list_url));

        assert!(is_apt_request(release_url));
        assert!(is_apt_package_list(release_url));
        assert!(!is_apt_package_file(release_url));
    }
}
