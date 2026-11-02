use std::{fs, os::unix::fs::PermissionsExt};

use bollard_buildkit_proto::{
    fsutil::types::{Packet, packet::PacketType},
    moby::filesync::v1::{file_sync_client::FileSyncClient, file_sync_server::FileSyncServer},
};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};

use super::{Filter, GO_MODE_DIR, GO_MODE_SYMLINK, LocalSources, Pattern, walk};

/// A context with a nested directory, a symlink, `.git` and a plan file.
fn context() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/lib")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("node_modules/left-pad")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref").unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("server.js"), "x".repeat(70_000)).unwrap();
    fs::write(root.join("src/lib/a.js"), "a").unwrap();
    fs::write(root.join("src/b.js"), "b").unwrap();
    fs::write(root.join("node_modules/left-pad/index.js"), "pad").unwrap();
    fs::write(root.join(".plan.json"), "{\"steps\":[]}").unwrap();
    fs::set_permissions(root.join("server.js"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("server.js", root.join("main.js")).unwrap();
    dir
}

fn paths(filter: &Filter, root: &std::path::Path) -> Vec<String> {
    walk(root, filter).unwrap().into_iter().map(|entry| entry.stat.path).collect()
}

#[test]
fn walks_parents_first_in_byte_order_without_git() {
    let dir = context();
    let entries = walk(dir.path(), &Filter::default()).unwrap();
    let names: Vec<&str> = entries.iter().map(|entry| entry.stat.path.as_str()).collect();
    assert_eq!(
        names,
        [
            ".plan.json",
            "main.js",
            "node_modules",
            "node_modules/left-pad",
            "node_modules/left-pad/index.js",
            "package.json",
            "server.js",
            "src",
            "src/b.js",
            "src/lib",
            "src/lib/a.js",
        ]
    );
    let find = |path: &str| entries.iter().find(|entry| entry.stat.path == path).unwrap();
    assert_eq!(find("src").stat.mode, GO_MODE_DIR | 0o755 & find("src").stat.mode);
    assert!(find("src").file.is_none());
    assert_eq!((find("server.js").stat.mode, find("server.js").stat.size), (0o755, 70_000));
    assert_eq!(find("main.js").stat.mode & GO_MODE_SYMLINK, GO_MODE_SYMLINK);
    assert_eq!(find("main.js").stat.linkname, "server.js");
    assert!(find("main.js").file.is_none(), "a symlink has no data to send");
    assert_eq!((find("package.json").stat.uid, find("package.json").stat.gid), (0, 0));
}

#[test]
fn filters_like_buildkit_asks() {
    let dir = context();
    let root = dir.path();
    let strings = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();

    // The plan alone, as the "dockerfile" source asks with followpaths.
    let plan = Filter::new(&[], &strings(&[".plan.json"]), &[]);
    assert_eq!(paths(&plan, root), [".plan.json"]);
    let nested = Filter::new(&[], &strings(&["src/lib/a.js"]), &[]);
    assert_eq!(paths(&nested, root), ["src", "src/lib", "src/lib/a.js"]);

    // Excluding the plan and node_modules, re-including one file under an excluded directory.
    let excluded = Filter::new(&[], &[], &strings(&[".plan.json", "node_modules", "src", "!src/lib/a.js"]));
    assert_eq!(paths(&excluded, root), ["main.js", "package.json", "server.js", "src", "src/lib", "src/lib/a.js"]);
    let globs = Filter::new(&[], &[], &strings(&["**/*.js", "!server.js"]));
    assert_eq!(
        paths(&globs, root),
        [".plan.json", "node_modules", "node_modules/left-pad", "package.json", "server.js", "src", "src/lib"]
    );
}

#[test]
fn globs_match_like_go() {
    let matches = |pattern: &str, path: &str| Pattern::new(pattern).unwrap().matches_or_parent(path);
    assert!(matches("*.js", "a.js") && !matches("*.js", "src/a.js"));
    assert!(matches("src", "src/lib/a.js"), "a directory matches what is under it");
    assert!(matches("**/a.js", "src/lib/a.js") && matches("**/a.js", "a.js"));
    assert!(matches("src/**/*.js", "src/lib/a.js") && matches("./src/?.js", "src/b.js"));
    assert!(matches("[a-c].js", "b.js") && !matches("[!a-c].js", "b.js") && matches("[^x]?js", "b.js"));
    assert!(matches("\\*.js", "*.js") && !matches("\\*.js", "a.js"));
    assert!(Pattern::new("./").is_none());
}

#[tokio::test]
async fn shares_a_local_source_over_diff_copy() {
    let dir = context();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tonic::transport::Server::builder()
        .add_service(FileSyncServer::new(LocalSources { root: dir.path().to_path_buf() }))
        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener));
    tokio::spawn(server);
    let mut client = FileSyncClient::connect(format!("http://{address}")).await.unwrap();

    let (requests, outgoing) = tokio::sync::mpsc::channel(8);
    let mut request = tonic::Request::new(ReceiverStream::new(outgoing));
    request.metadata_mut().insert("dir-name", "context".parse().unwrap());
    for pattern in [".plan.json", "node_modules"] {
        request.metadata_mut().append("exclude-patterns", pattern.parse().unwrap());
    }
    let mut incoming = client.diff_copy(request).await.unwrap().into_inner();

    // The stats, then the empty one that ends them.
    let mut stats = Vec::new();
    loop {
        let packet = incoming.next().await.unwrap().unwrap();
        assert_eq!(packet.r#type, PacketType::PacketStat as i32);
        match packet.stat {
            Some(stat) => stats.push(stat.path),
            None => break,
        }
    }
    assert_eq!(stats, ["main.js", "package.json", "server.js", "src", "src/b.js", "src/lib", "src/lib/a.js"]);

    // A file larger than one packet, then a small one.
    let mut data = Vec::new();
    for (id, expected) in [(2u32, "x".repeat(70_000)), (4, "b".to_string())] {
        requests.send(Packet { r#type: PacketType::PacketReq.into(), id, ..Default::default() }).await.unwrap();
        let mut received = Vec::new();
        loop {
            let packet = incoming.next().await.unwrap().unwrap();
            assert_eq!((packet.r#type, packet.id), (PacketType::PacketData as i32, id));
            if packet.data.is_empty() {
                break;
            }
            data.push(packet.data.len());
            received.extend(packet.data);
        }
        assert_eq!(String::from_utf8(received).unwrap(), expected);
    }
    assert!(data.iter().all(|len| *len <= super::CHUNK));

    requests.send(Packet { r#type: PacketType::PacketFin.into(), ..Default::default() }).await.unwrap();
    let fin = incoming.next().await.unwrap().unwrap();
    assert_eq!(fin.r#type, PacketType::PacketFin as i32);
    assert!(incoming.next().await.is_none(), "the stream ends after FIN");

    // Another source is refused.
    let mut other = tonic::Request::new(tokio_stream::empty::<Packet>());
    other.metadata_mut().insert("dir-name", "secrets".parse().unwrap());
    assert_eq!(client.diff_copy(other).await.unwrap_err().code(), tonic::Code::NotFound);
}
