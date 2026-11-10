use std::collections::BTreeMap;
use std::io::Read;

use super::*;

/// The stat header podman would send: base64 JSON.
fn header(mode: u32, is_dir: bool, link_target: &str) -> String {
    BASE64.encode(format!(
        r#"{{"name":"x","size":1,"mode":{mode},"mtime":"2026-11-10T16:54:45+01:00","isDir":{is_dir},"linkTarget":"{link_target}"}}"#
    ))
}

const DIR: u32 = (1 << 31) | 0o755;
const FILE: u32 = 0o644;
const LINK: u32 = (1 << 27) | 0o777;

/// The answers of podman 6.1.4 (architecture §1.7), as captured from `alpine:3.22`.
#[test]
fn path_stats_are_read_from_status_and_header() {
    let stat = |path: &str, status: u16, header: Option<String>| {
        PathStat::from_response(path, status, header.as_deref())
    };
    assert_eq!(stat("/nope", 404, None), Ok(PathStat::Missing));
    assert_eq!(
        stat("/etc", 200, Some(header(DIR, true, "/etc"))),
        Ok(PathStat::Found {
            is_dir: true,
            is_symlink: false,
            resolved: "/etc".into()
        })
    );
    // The root is written `/.`.
    assert_eq!(
        stat("/", 200, Some(header(DIR, true, "/."))),
        Ok(PathStat::Found {
            is_dir: true,
            is_symlink: false,
            resolved: "/".into()
        })
    );
    // A link to a directory, and one to a file.
    assert_eq!(
        stat("/var/run", 200, Some(header(LINK, true, "/run"))),
        Ok(PathStat::Found {
            is_dir: true,
            is_symlink: true,
            resolved: "/run".into()
        })
    );
    assert_eq!(
        stat(
            "/etc/os-release",
            200,
            Some(header(LINK, false, "/usr/lib/os-release"))
        ),
        Ok(PathStat::Found {
            is_dir: false,
            is_symlink: true,
            resolved: "/usr/lib/os-release".into()
        })
    );
    // A file below a linked directory is not itself a link.
    assert_eq!(
        stat(
            "/var/run/app.pid",
            200,
            Some(header(FILE, false, "/run/app.pid"))
        ),
        Ok(PathStat::Found {
            is_dir: false,
            is_symlink: false,
            resolved: "/run/app.pid".into()
        })
    );
    // A dangling link is a 404 that still carries the stat.
    assert_eq!(
        stat(
            "/var/log/nginx/access.log",
            404,
            Some(header(LINK, false, "/dev/stdout"))
        ),
        Ok(PathStat::Dangling {
            target: "/dev/stdout".into()
        })
    );
    assert!(
        stat("/etc", 500, None)
            .unwrap_err()
            .contains("answered 500")
    );
    assert!(stat("/etc", 200, None).is_err());
    assert!(stat("/etc", 200, Some("not base64!".into())).is_err());
    assert!(stat("/etc", 200, Some(BASE64.encode("not json"))).is_err());
}

/// A container's file system as a table of what a stat of each path answers; anything else
/// is missing.
#[derive(Default)]
struct Image(BTreeMap<&'static str, PathStat>);

impl Image {
    fn dir(mut self, path: &'static str) -> Self {
        self.0.insert(path, found(true, false, path));
        self
    }

    fn file(mut self, path: &'static str) -> Self {
        self.0.insert(path, found(false, false, path));
        self
    }

    /// `path` is a symbolic link to the existing `target`.
    fn link(mut self, path: &'static str, target: &'static str, is_dir: bool) -> Self {
        self.0.insert(path, found(is_dir, true, target));
        self
    }

    fn dangling(mut self, path: &'static str, target: &'static str) -> Self {
        self.0.insert(
            path,
            PathStat::Dangling {
                target: target.into(),
            },
        );
        self
    }

    fn with(mut self, path: &'static str, stat: PathStat) -> Self {
        self.0.insert(path, stat);
        self
    }
}

fn found(is_dir: bool, is_symlink: bool, resolved: &str) -> PathStat {
    PathStat::Found {
        is_dir,
        is_symlink,
        resolved: resolved.into(),
    }
}

impl StatPath for Image {
    async fn stat(&self, path: &str) -> Result<PathStat, RuntimeError> {
        Ok(self.0.get(path).cloned().unwrap_or(PathStat::Missing))
    }
}

fn file(path: &str, owner: Ownership) -> ContainerFile {
    ContainerFile {
        path: path.parse().unwrap(),
        content: format!("content of {path}\n").as_str().into(),
        mode: "0440".parse().unwrap(),
        owner,
    }
}

fn root_owned(path: &str) -> ContainerFile {
    file(path, Ownership::Ids(FileOwner::ROOT))
}

/// The layout of `files` in `image` with the volumes `volumes`: its directories and resolved
/// file paths, or the refusal.
async fn layout(
    image: &Image,
    files: &[ContainerFile],
    volumes: &[&str],
) -> Result<(Vec<String>, Vec<String>), String> {
    let volumes: Vec<String> = volumes.iter().map(|v| (*v).to_owned()).collect();
    match Layout::resolve(files, &volumes, image).await {
        Ok(layout) => Ok((
            layout.dirs.iter().cloned().collect(),
            layout.files.keys().cloned().collect(),
        )),
        Err(RuntimeError::ConfigFiles { message }) => Err(message),
        Err(other) => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn missing_parents_are_created_and_existing_ones_left_alone() {
    let image = Image::default().dir("/etc").file("/etc/app.conf");
    let files = [
        root_owned("/etc/app.conf"),
        root_owned("/etc/app/conf.d/a.yml"),
        root_owned("/etc/app/b.yml"),
        root_owned("/opt/x/y"),
        root_owned("/top"),
    ];
    let (dirs, paths) = layout(&image, &files, &[]).await.unwrap();
    assert_eq!(dirs, ["/etc/app", "/etc/app/conf.d", "/opt", "/opt/x"]);
    assert_eq!(
        paths,
        [
            "/etc/app.conf",
            "/etc/app/b.yml",
            "/etc/app/conf.d/a.yml",
            "/opt/x/y",
            "/top"
        ]
    );
}

#[tokio::test]
async fn linked_directories_are_followed() {
    // `/var/run` → `/run`, as in most images; `/run/app` exists, `/run/app/conf` does not.
    let image = Image::default()
        .dir("/var")
        .link("/var/run", "/run", true)
        .with("/var/run/app", found(true, false, "/run/app"))
        .with(
            "/var/run/existing.conf",
            found(false, false, "/run/existing.conf"),
        );
    let files = [
        root_owned("/var/run/app/conf/a.yml"),
        root_owned("/var/run/existing.conf"),
    ];
    let (dirs, paths) = layout(&image, &files, &[]).await.unwrap();
    assert_eq!(dirs, ["/run/app/conf"]);
    assert_eq!(paths, ["/run/app/conf/a.yml", "/run/existing.conf"]);

    // The declared path looked fine at apply; where it really goes is a volume.
    let refused = layout(&image, &files[..1], &["/run"]).await.unwrap_err();
    assert_eq!(
        refused,
        "`/var/run/app/conf/a.yml` is `/run/app/conf/a.yml` in the image: \
         `/run/app/conf/a.yml` is in the volume mounted at `/run`; a volume hides files \
         uploaded there"
    );
    // Or a reserved place.
    let image = Image::default().link("/host", "/proc/1", true);
    let refused = layout(&image, &[root_owned("/host/environ")], &[])
        .await
        .unwrap_err();
    assert!(
        refused.starts_with("`/host/environ` is `/proc/1/environ` in the image: "),
        "{refused}"
    );
    // A link to the root resolves below it.
    let image = Image::default().link("/rootfs", "/", true);
    let (dirs, paths) = layout(&image, &[root_owned("/rootfs/etc/a")], &[])
        .await
        .unwrap();
    assert_eq!(
        (dirs, paths),
        (vec!["/etc".to_owned()], vec!["/etc/a".to_owned()])
    );
}

#[tokio::test]
async fn links_directories_and_files_in_the_way_are_refused_by_name() {
    let image = Image::default()
        .dir("/etc")
        .dir("/etc/apk")
        .file("/etc/motd")
        .link("/etc/os-release", "/usr/lib/os-release", false)
        .dangling("/var/log/nginx/access.log", "/dev/stdout")
        .dangling("/srv", "/mnt/gone");
    let refusal = async |path: &str| layout(&image, &[root_owned(path)], &[]).await.unwrap_err();
    assert_eq!(
        refusal("/etc/os-release").await,
        "`/etc/os-release` is a symbolic link in the image (to `/usr/lib/os-release`); a \
         config file cannot replace a link"
    );
    assert_eq!(
        refusal("/var/log/nginx/access.log").await,
        "`/var/log/nginx/access.log` is a symbolic link in the image (to `/dev/stdout`); a \
         config file cannot replace a link"
    );
    assert_eq!(
        refusal("/etc/apk").await,
        "`/etc/apk` is a directory in the image"
    );
    assert_eq!(
        refusal("/etc/motd/x.conf").await,
        "`/etc/motd/x.conf` is below `/etc/motd`, which is a file in the image"
    );
    assert_eq!(
        refusal("/srv/www/x.conf").await,
        "`/srv/www/x.conf` is below `/srv`, a symbolic link in the image to `/mnt/gone`, which \
         does not exist"
    );
}

#[tokio::test]
async fn two_files_cannot_meet_through_a_link() {
    let image = Image::default()
        .dir("/run")
        .link("/var/run", "/run", true)
        .dir("/var");
    let same = [root_owned("/run/a.conf"), root_owned("/var/run/a.conf")];
    assert_eq!(
        layout(&image, &same, &[]).await.unwrap_err(),
        "`/run/a.conf` and `/var/run/a.conf` are the same file in the image, `/run/a.conf`"
    );
    let nested = [root_owned("/run/app"), root_owned("/var/run/app/a.conf")];
    assert_eq!(
        layout(&image, &nested, &[]).await.unwrap_err(),
        "`/run/app` is `/run/app` in the image, which another config file needs as a directory"
    );
}

/// The entries of a tar archive: path, type, uid, gid, mode, mtime and content.
fn entries(archive: &[u8]) -> Vec<(String, char, u64, u64, u32, u64, String)> {
    tar::Archive::new(archive)
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            (
                entry.path().unwrap().to_string_lossy().into_owned(),
                if header.entry_type().is_dir() {
                    'd'
                } else {
                    'f'
                },
                header.uid().unwrap(),
                header.gid().unwrap(),
                header.mode().unwrap(),
                header.mtime().unwrap(),
                content,
            )
        })
        .collect()
}

/// Architecture §8: directories and numeric owners first, the container user's files apart.
#[tokio::test]
async fn archives_split_numeric_owners_from_the_container_user() {
    let image = Image::default().dir("/etc");
    let files = [
        root_owned("/etc/app/config.yml"),
        file(
            "/etc/app/shared.key",
            Ownership::Ids(FileOwner { uid: 0, gid: 65534 }),
        ),
        file("/run/secrets/creds.json", Ownership::ContainerUser),
    ];
    let layout = Layout::resolve(&files, &[], &image).await.unwrap();
    let archives = layout.archives(1_800_000_000).unwrap();

    let ids = entries(archives.ids.as_deref().unwrap());
    let summary: Vec<_> = ids
        .iter()
        .map(|(path, kind, uid, gid, mode, ..)| format!("{kind} {path} {uid}:{gid} {mode:04o}"))
        .collect();
    assert_eq!(
        summary,
        [
            // Every missing parent, also the container user's, as root's: `copyUIDGID` would
            // hand the ones it creates to that user.
            "d etc/app/ 0:0 0755",
            "d run/ 0:0 0755",
            "d run/secrets/ 0:0 0755",
            "f etc/app/config.yml 0:0 0440",
            "f etc/app/shared.key 0:65534 0440",
        ]
    );
    assert!(ids.iter().all(|entry| entry.5 == 1_800_000_000));
    assert_eq!(ids[3].6, "content of /etc/app/config.yml\n");

    let user = entries(archives.container_user.as_deref().unwrap());
    assert_eq!(user.len(), 1);
    assert_eq!(user[0].0, "run/secrets/creds.json");
    assert_eq!((user[0].1, user[0].4), ('f', 0o440));
    assert_eq!(user[0].6, "content of /run/secrets/creds.json\n");
}

#[tokio::test]
async fn empty_archives_are_not_sent() {
    let image = Image::default().dir("/etc").dir("/run");
    let only_ids = [root_owned("/etc/a.conf")];
    let archives = Layout::resolve(&only_ids, &[], &image)
        .await
        .unwrap()
        .archives(0)
        .unwrap();
    assert!(archives.ids.is_some() && archives.container_user.is_none());

    // No missing directory and no numeric owner: only the container user's upload.
    let only_user = [file("/run/creds", Ownership::ContainerUser)];
    let archives = Layout::resolve(&only_user, &[], &image)
        .await
        .unwrap()
        .archives(0)
        .unwrap();
    assert!(archives.ids.is_none() && archives.container_user.is_some());
}

#[tokio::test]
async fn long_paths_survive_the_archive() {
    let long = format!("/etc/{}/{}.conf", "d".repeat(120), "f".repeat(120));
    let files = [root_owned(&long)];
    let image = Image::default().dir("/etc");
    let archives = Layout::resolve(&files, &[], &image)
        .await
        .unwrap()
        .archives(0)
        .unwrap();
    let ids = entries(archives.ids.as_deref().unwrap());
    assert_eq!(ids.last().unwrap().0, long[1..]);
}

/// podman's messages as captured on 6.1.4, and the dump form the spike saw (architecture §1.7).
#[test]
fn upload_failures_show_only_the_copier_reason() {
    let ownership = "copier: put: error setting ownership of \"/etc/app/b.conf\" to 70000:0: \
                     lchown /etc/app/b.conf: invalid argument";
    assert_eq!(
        UploadFailure(ownership).reason(),
        "error setting ownership of \"/etc/app/b.conf\" to 70000:0: lchown /etc/app/b.conf: \
         invalid argument"
    );
    let over_directory = "copier: put: error creating \"/etc/apk\": copier: put: error creating \
                          file at \"/etc/apk\": open /etc/apk: file exists";
    assert_eq!(
        UploadFailure(over_directory).reason(),
        "error creating file at \"/etc/apk\": open /etc/apk: file exists"
    );
    let dump = "error copying to container: copier.request{Request:\"PUT\", \
                Root:\"/home/hoster/.local/share/containers/storage/overlay/abc/merged\", \
                Directory:\"/home/hoster/.local/share/containers/storage/overlay/abc/merged\"}: \
                copier: put: error setting ownership of \"/x\" to 70000:0: invalid argument\n\
                more: /home/hoster/secret/path";
    let reason = UploadFailure(dump).reason();
    assert_eq!(
        reason,
        "error setting ownership of \"/x\" to 70000:0: invalid argument"
    );
    // Without the marker nothing of podman's text is shown.
    let unknown = "something about /home/hoster/.local/share/containers/storage went wrong";
    assert_eq!(UploadFailure(unknown).reason(), "podman refused the upload");
    assert_eq!(
        UploadFailure("copier: put: ").reason(),
        "podman refused the upload"
    );
    let long = format!("copier: put: {}", "x".repeat(5000));
    assert_eq!(UploadFailure(&long).reason().chars().count(), 300);

    let error = RuntimeError::from(UploadFailure(ownership));
    assert!(
        error
            .to_string()
            .starts_with("cannot write its config files: error setting ownership"),
        "{error}"
    );
}

#[test]
fn a_container_file_never_prints_its_content() {
    let file = file("/etc/app.conf", Ownership::ContainerUser);
    let debug = format!("{file:?}");
    assert!(debug.contains("/etc/app.conf"), "{debug}");
    assert!(!debug.contains("content of"), "{debug}");
}
