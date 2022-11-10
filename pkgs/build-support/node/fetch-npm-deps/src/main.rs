#![warn(clippy::pedantic)]

use crate::cacache::Cache;
use anyhow::{anyhow, Context};
use rayon::prelude::*;
use serde::Deserialize;
use std::{
    collections::HashMap,
    env, fmt, fs,
    path::Path,
    process::{self, Command},
};
use tempfile::tempdir;
use url::Url;

mod cacache;

#[derive(Deserialize)]
struct PackageLock {
    #[serde(rename = "lockfileVersion")]
    version: u8,
    dependencies: Option<HashMap<String, OldPackage>>,
    packages: Option<HashMap<String, Package>>,
}

#[derive(Deserialize)]
struct OldPackage {
    version: UrlOrString,
    resolved: Option<UrlOrString>,
    integrity: Option<String>,
    dependencies: Option<HashMap<String, OldPackage>>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Package {
    resolved: Option<UrlOrString>,
    integrity: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum UrlOrString {
    Url(Url),
    String(String),
}

impl fmt::Display for UrlOrString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrlOrString::Url(url) => url.fmt(f),
            UrlOrString::String(string) => string.fmt(f),
        }
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn to_new_packages(
    old_packages: HashMap<String, OldPackage>,
    initial_url: &Url,
) -> anyhow::Result<HashMap<String, Package>> {
    let mut new = HashMap::new();

    for (name, mut package) in old_packages {
        if let UrlOrString::Url(v) = &package.version {
            for (scheme, host) in [
                ("github", "github.com"),
                ("bitbucket", "bitbucket.org"),
                ("gitlab", "gitlab.com"),
            ] {
                if v.scheme() == scheme {
                    package.version = {
                        let mut new_url = initial_url.clone();

                        new_url.set_host(Some(host))?;

                        if v.path().ends_with(".git") {
                            new_url.set_path(v.path());
                        } else {
                            new_url.set_path(&format!("{}.git", v.path()));
                        }

                        new_url.set_fragment(v.fragment());

                        UrlOrString::Url(new_url)
                    };

                    break;
                }
            }
        }

        new.insert(
            format!("{name}-{}", package.version),
            Package {
                resolved: if matches!(package.version, UrlOrString::Url(_)) {
                    Some(package.version)
                } else {
                    package.resolved
                },
                integrity: package.integrity,
            },
        );

        if let Some(dependencies) = package.dependencies {
            new.extend(to_new_packages(dependencies, initial_url)?);
        }
    }

    Ok(new)
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn get_hosted_git_url(url: &Url) -> Option<Url> {
    if ["git", "http", "git+ssh", "git+https", "ssh", "https"].contains(&url.scheme()) {
        let mut s = url.path_segments()?;

        match url.host_str()? {
            "github.com" => {
                let user = s.next()?;
                let mut project = s.next()?;
                let typ = s.next();
                let mut commit = s.next();

                if typ.is_none() {
                    commit = url.fragment();
                } else if typ.is_some() && typ != Some("tree") {
                    return None;
                }

                if project.ends_with(".git") {
                    project = project.strip_suffix(".git")?;
                }

                let commit = commit.unwrap();

                Some(
                    Url::parse(&format!(
                        "https://codeload.github.com/{user}/{project}/tar.gz/{commit}"
                    ))
                    .ok()?,
                )
            }
            "bitbucket.org" => {
                let user = s.next()?;
                let mut project = s.next()?;
                let aux = s.next();

                if aux == Some("get") {
                    return None;
                }

                if project.ends_with(".git") {
                    project = project.strip_suffix(".git")?;
                }

                let commit = url.fragment()?;

                Some(
                    Url::parse(&format!(
                        "https://bitbucket.org/{user}/{project}/get/{commit}.tar.gz"
                    ))
                    .ok()?,
                )
            }
            "gitlab.com" => {
                let path = &url.path()[1..];

                if path.contains("/~/") || path.contains("/archive.tar.gz") {
                    return None;
                }

                let user = s.next()?;
                let mut project = s.next()?;

                if project.ends_with(".git") {
                    project = project.strip_suffix(".git")?;
                }

                let commit = url.fragment()?;

                Some(
                    Url::parse(&format!(
                    "https://gitlab.com/{user}/{project}/repository/archive.tar.gz?ref={commit}"
                ))
                    .ok()?,
                )
            }
            "git.sr.ht" => {
                let user = s.next()?;
                let mut project = s.next()?;
                let aux = s.next();

                if aux == Some("archive") {
                    return None;
                }

                if project.ends_with(".git") {
                    project = project.strip_suffix(".git")?;
                }

                let commit = url.fragment()?;

                Some(
                    Url::parse(&format!(
                        "https://git.sr.ht/{user}/{project}/archive/{commit}.tar.gz"
                    ))
                    .ok()?,
                )
            }
            _ => None,
        }
    } else {
        None
    }
}

fn get_ideal_hash(integrity: &str) -> anyhow::Result<&str> {
    let split: Vec<_> = integrity.split_ascii_whitespace().collect();

    if split.len() == 1 {
        Ok(split[0])
    } else {
        for hash in ["sha512-", "sha1-"] {
            if let Some(h) = split.iter().find(|s| s.starts_with(hash)) {
                return Ok(h);
            }
        }

        Err(anyhow!("not sure which hash to select out of {split:?}"))
    }
}

fn get_initial_url() -> anyhow::Result<Url> {
    Url::parse("git+ssh://git@a.b").context("initial url should be valid")
}

fn main() -> anyhow::Result<()> {
    let args = env::args().collect::<Vec<_>>();

    if args.len() < 2 {
        println!("usage: {} <path/to/package-lock.json>", args[0]);
        println!();
        println!("Prefetches npm dependencies for usage by fetchNpmDeps.");

        process::exit(1);
    }

    let lock_content = fs::read_to_string(&args[1])?;
    let lock: PackageLock = serde_json::from_str(&lock_content)?;

    let out_tempdir;

    let (out, print_hash) = if let Some(path) = args.get(2) {
        (Path::new(path), false)
    } else {
        out_tempdir = tempdir()?;

        (out_tempdir.path(), true)
    };

    let agent = ureq::agent();

    eprintln!("lockfile version: {}", lock.version);

    let packages = match lock.version {
        1 => {
            let initial_url = get_initial_url()?;

            lock.dependencies
                .map(|p| to_new_packages(p, &initial_url))
                .transpose()?
        }
        2 | 3 => lock.packages,
        _ => panic!(
            "We don't support lockfile version {}, please file an issue.",
            lock.version
        ),
    };

    if packages.is_none() {
        return Ok(());
    }

    let cache = Cache::new(out.join("_cacache"));

    packages
        .unwrap()
        .into_par_iter()
        .filter(|(dep, _)| !dep.is_empty())
        .filter(|(_, package)| matches!(package.resolved, Some(UrlOrString::Url(_))))
        .try_for_each(|(dep, package)| {
            eprintln!("{dep}");

            let mut resolved = match package.resolved {
                Some(UrlOrString::Url(url)) => url,
                _ => unreachable!(),
            };

            if let Some(hosted_git_url) = get_hosted_git_url(&resolved) {
                resolved = hosted_git_url;
            }

            let mut data = Vec::new();

            agent
                .get(resolved.as_str())
                .call()?
                .into_reader()
                .read_to_end(&mut data)?;

            cache
                .put(
                    format!("make-fetch-happen:request-cache:{resolved}"),
                    resolved,
                    &data,
                    package
                        .integrity
                        .map(|i| Ok::<String, anyhow::Error>(get_ideal_hash(&i)?.to_string()))
                        .transpose()?,
                )
                .map_err(|e| anyhow!("couldn't insert cache entry for {dep}: {e:?}"))?;

            Ok::<_, anyhow::Error>(())
        })?;

    fs::write(out.join("package-lock.json"), lock_content)?;

    if print_hash {
        Command::new("nix")
            .args(["--experimental-features", "nix-command", "hash", "path"])
            .arg(out.as_os_str())
            .status()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        get_hosted_git_url, get_ideal_hash, get_initial_url, to_new_packages, OldPackage, Package,
        UrlOrString,
    };
    use std::collections::HashMap;
    use url::Url;

    #[test]
    fn hosted_git_urls() {
        for (input, expected) in [
            (
                "git+ssh://git@github.com/castlabs/electron-releases.git#fc5f78d046e8d7cdeb66345a2633c383ab41f525",
                Some("https://codeload.github.com/castlabs/electron-releases/tar.gz/fc5f78d046e8d7cdeb66345a2633c383ab41f525"),
            ),
            (
                "https://user@github.com/foo/bar#fix/bug",
                Some("https://codeload.github.com/foo/bar/tar.gz/fix/bug")
            ),
            (
                "https://github.com/eligrey/classList.js/archive/1.2.20180112.tar.gz",
                None
            ),
            (
                "git+ssh://bitbucket.org/foo/bar#branch",
                Some("https://bitbucket.org/foo/bar/get/branch.tar.gz")
            ),
            (
                "ssh://git@gitlab.com/foo/bar.git#fix/bug",
                Some("https://gitlab.com/foo/bar/repository/archive.tar.gz?ref=fix/bug")
            ),
            (
                "git+ssh://git.sr.ht/~foo/bar#branch",
                Some("https://git.sr.ht/~foo/bar/archive/branch.tar.gz")
            ),
        ] {
            assert_eq!(
                get_hosted_git_url(&Url::parse(input).unwrap()),
                expected.map(|u| Url::parse(u).unwrap())
            );
        }
    }

    #[test]
    fn ideal_hashes() {
        for (input, expected) in [
            ("sha512-foo sha1-bar", Some("sha512-foo")),
            ("sha1-bar md5-foo", Some("sha1-bar")),
            ("sha1-bar", Some("sha1-bar")),
            ("sha512-foo", Some("sha512-foo")),
            ("foo-bar sha1-bar", Some("sha1-bar")),
            ("foo-bar baz-foo", None),
        ] {
            assert_eq!(get_ideal_hash(input).ok(), expected);
        }
    }

    #[test]
    fn git_shorthand_v1() -> anyhow::Result<()> {
        let old =
            {
                let mut o = HashMap::new();
                o.insert(
                String::from("sqlite3"),
                OldPackage {
                    version: UrlOrString::Url(Url::parse(
                        "github:mapbox/node-sqlite3#593c9d498be2510d286349134537e3bf89401c4a",
                    ).unwrap()),
                    resolved: None,
                    integrity: None,
                    dependencies: None,
                },
            );
                o
            };

        let initial_url = get_initial_url()?;

        let new = to_new_packages(old, &initial_url)?;

        assert_eq!(new.len(), 1, "new packages map should contain 1 value");
        assert_eq!(new.into_values().next().unwrap(), Package {
            resolved: Some(UrlOrString::Url(Url::parse("git+ssh://git@github.com/mapbox/node-sqlite3.git#593c9d498be2510d286349134537e3bf89401c4a").unwrap())),
            integrity: None
        });

        Ok(())
    }
}
