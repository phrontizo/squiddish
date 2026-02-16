/// APT-specific optimizations and cache handling

/// Detect if a request is an APT package manager request
pub fn is_apt_request(uri: &str) -> bool {
    // Check for Debian/Ubuntu repository patterns
    let apt_patterns = [
        "deb.debian.org",
        "archive.ubuntu.com",
        "security.ubuntu.com",
        "security.debian.org",
        "/ubuntu/",
        "/debian/",
        "Packages.gz",
        "Packages.xz",
        "Release",
        "Release.gpg",
        "InRelease",
        ".deb",
    ];

    apt_patterns.iter().any(|pattern| uri.contains(pattern))
}

/// Determine if an APT request is a package list (should be cached shorter)
pub fn is_apt_package_list(uri: &str) -> bool {
    let list_patterns = ["Packages.gz", "Packages.xz", "Release", "InRelease"];
    list_patterns.iter().any(|pattern| uri.contains(pattern))
}

/// Determine if an APT request is a package file (can be cached longer)
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
        assert!(!is_apt_request("http://example.com/file.tar.gz"));
    }

    #[test]
    fn test_is_apt_package_list() {
        assert!(is_apt_package_list("http://archive.ubuntu.com/ubuntu/dists/focal/main/binary-amd64/Packages.gz"));
        assert!(is_apt_package_list("http://archive.ubuntu.com/ubuntu/dists/focal/Release"));
        assert!(!is_apt_package_list("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
    }

    #[test]
    fn test_is_apt_package_file() {
        assert!(is_apt_package_file("http://archive.ubuntu.com/ubuntu/pool/main/a/apache2/apache2_2.4.41-4ubuntu3_amd64.deb"));
        assert!(!is_apt_package_file("http://archive.ubuntu.com/ubuntu/dists/focal/Release"));
    }
}
