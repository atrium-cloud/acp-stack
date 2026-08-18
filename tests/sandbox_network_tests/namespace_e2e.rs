use std::process::{Command, Stdio};
use std::time::Duration;

use crate::support::*;

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn namespace_handle_stays_usable_during_teardown() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    require_capability(bin_available("nsenter", "--version"), "nsenter unavailable");
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    // Teardown runs after the workload (and the whole unshare chain) has died;
    // entering the namespace still works because the supervisor's fd holds it.
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = teardown ]; then\n\
         nsenter --net=\"$ACPS_SANDBOX_NETWORK_NAMESPACE\" true || exit 1\n\
         touch \"$markdir/teardown-entered-ns\"\nfi\nexit 0",
    );

    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &["/bin/true"],
    )
    .status()
    .expect("run supervise");
    assert_eq!(status.code(), Some(0));
    assert!(markdir.join("teardown-entered-ns").exists());
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn workload_namespace_matches_supervisor_handle_and_ids_are_unique() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = setup ]; then\n\
         readlink \"$ACPS_SANDBOX_NETWORK_NAMESPACE\" > \"$markdir/handle-ns-$ACPS_SANDBOX_NETWORK_ID\"\n\
         echo \"$ACPS_SANDBOX_NETWORK_ID\" >> \"$markdir/ids\"\nfi\nexit 0",
    );
    let provider_args = [provider.to_str().unwrap(), markdir.to_str().unwrap()];

    // Two overlapping spawns: distinct IDs, distinct namespaces, and each
    // workload's own ns/net equals the handle its supervisor captured.
    let spawn = |tag: &str| {
        supervise_command(
            &provider_args,
            "10s",
            "daemon",
            &[
                "/bin/sh",
                "-c",
                &format!(
                    "readlink /proc/self/ns/net > {}/workload-ns-{tag} && sleep 1",
                    markdir.display()
                ),
            ],
        )
        .spawn()
        .expect("spawn supervise")
    };
    let mut first = spawn("a");
    let mut second = spawn("b");
    assert!(first.wait().expect("wait first").success());
    assert!(second.wait().expect("wait second").success());

    let ids: Vec<String> = std::fs::read_to_string(markdir.join("ids"))
        .expect("ids file")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "concurrent spawns must get distinct IDs");

    let mut handle_inodes: Vec<String> = ids
        .iter()
        .map(|id| {
            std::fs::read_to_string(markdir.join(format!("handle-ns-{id}")))
                .expect("handle ns dump")
                .trim()
                .to_owned()
        })
        .collect();
    let mut workload_inodes: Vec<String> = ["a", "b"]
        .iter()
        .map(|tag| {
            std::fs::read_to_string(markdir.join(format!("workload-ns-{tag}")))
                .expect("workload ns dump")
                .trim()
                .to_owned()
        })
        .collect();
    assert_ne!(
        workload_inodes[0], workload_inodes[1],
        "concurrent spawns must get distinct namespaces"
    );
    handle_inodes.sort();
    workload_inodes.sort();
    assert_eq!(
        handle_inodes, workload_inodes,
        "the supervisor handle must name the workload's own netns"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn supervisor_stays_on_the_host_network_while_the_workload_is_isolated() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    let workload_ns = markdir.join("workload-ns");
    // The supervisor is the process a prober reaches first, and it is NOT the
    // isolated one: only the leaf under `unshare --net` is. Probing the wrong
    // one reports isolation as broken when it is working.
    let mut child = supervise_command(
        &[],
        "10s",
        "daemon",
        &[
            "/bin/sh",
            "-c",
            &format!(
                "readlink /proc/self/ns/net > {} && sleep 5",
                workload_ns.display()
            ),
        ],
    )
    .spawn()
    .expect("spawn supervise");
    let supervisor_pid = child.id();

    assert!(
        wait_for_file(&workload_ns, Duration::from_secs(10)),
        "workload must report its netns"
    );
    let workload_inode = std::fs::read_to_string(&workload_ns)
        .expect("workload ns dump")
        .trim()
        .to_owned();
    let supervisor_inode =
        std::fs::read_link(format!("/proc/{supervisor_pid}/ns/net")).expect("supervisor netns");
    let host_inode = std::fs::read_link("/proc/self/ns/net").expect("host netns");

    assert_eq!(
        supervisor_inode, host_inode,
        "the supervisor must remain in the host network namespace"
    );
    assert_ne!(
        supervisor_inode.to_string_lossy(),
        workload_inode,
        "the workload must not share the supervisor's network namespace"
    );

    assert!(child.wait().expect("wait supervise").success());
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn deny_all_namespace_cannot_reach_a_parent_listener() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    require_capability(
        bin_available("bash", "--version"),
        "bash (for /dev/tcp) unavailable",
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind parent listener");
    let port = listener.local_addr().expect("listener addr").port();
    // Distinct sentinel exit codes so a tooling failure (bash missing, wrapper
    // error, exit 120/121) can never masquerade as "unreachable".
    let connect = format!("if exec 3<>/dev/tcp/127.0.0.1/{port}; then exit 42; else exit 43; fi");

    // Positive control: reachable from the parent namespace.
    let direct = Command::new("bash")
        .args(["-c", &connect])
        .status()
        .expect("run direct connect");
    assert_eq!(
        direct.code(),
        Some(42),
        "control connect must succeed outside"
    );

    // Isolated with no provider: deny-all, not even loopback.
    let status = supervise_command(&[], "10s", "daemon", &["bash", "-c", &connect])
        .status()
        .expect("run supervise");
    assert_eq!(
        status.code(),
        Some(43),
        "a deny-all namespace must fail the connect itself (not a tooling error)"
    );
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn veth_provider_enables_only_the_configured_endpoint() {
    require_capability(
        unshare_net_usable(),
        "unshare --net / CAP_SYS_ADMIN unavailable",
    );
    require_capability(
        bin_available("ip", "-V") && bin_available("nsenter", "--version"),
        "ip/nsenter unavailable",
    );
    // CAP_NET_ADMIN probe: creating (and removing) a throwaway link.
    let probe = Command::new("ip")
        .args([
            "link",
            "add",
            "acpsprobe0",
            "type",
            "veth",
            "peer",
            "name",
            "acpsprobe1",
        ])
        .stderr(Stdio::null())
        .status()
        .expect("run ip probe");
    if !probe.success() {
        require_capability(false, "CAP_NET_ADMIN unavailable (cannot create veth)");
    } else {
        let removed = Command::new("ip")
            .args(["link", "del", "acpsprobe0"])
            .status()
            .expect("remove ip probe");
        assert!(removed.success());
    }
    // Pre-clean a stale interface a previously panicked/killed run may have
    // left behind, so setup does not fail 120 for stale-state reasons.
    let precleaned = Command::new("ip")
        .args(["link", "del", "acpstest0"])
        .stderr(Stdio::null())
        .status()
        .expect("pre-clean stale veth");
    if precleaned.success() {
        eprintln!("note: removed stale acpstest0 from a previous run");
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let markdir = tmp.path();
    // Lifecycle provider: veth pair, child end moved into the namespace and
    // addressed; only 10.199.99.1 is reachable from inside. Teardown is
    // idempotent (`|| true`): namespace destruction usually removed the pair.
    let provider = write_provider_script(
        markdir,
        "if [ \"$phase\" = setup ]; then\n\
         ip link add acpstest0 type veth peer name acpstest1 || exit 1\n\
         ip link set acpstest1 netns \"$ACPS_SANDBOX_NETWORK_PID\" || exit 1\n\
         ip addr add 10.199.99.1/30 dev acpstest0 || exit 1\n\
         ip link set acpstest0 up || exit 1\n\
         nsenter --net=\"$ACPS_SANDBOX_NETWORK_NAMESPACE\" sh -c \
         'ip link set lo up && ip addr add 10.199.99.2/30 dev acpstest1 && ip link set acpstest1 up' || exit 1\n\
         fi\n\
         if [ \"$phase\" = teardown ]; then ip link del acpstest0 2>/dev/null || true; fi\n\
         exit 0",
    );

    // Sentinel codes prove which probe failed: the configured endpoint must
    // answer, and an address outside the provider's /30 must NOT be reachable
    // — the "only" half of the guarantee.
    let status = supervise_command(
        &[provider.to_str().unwrap(), markdir.to_str().unwrap()],
        "10s",
        "daemon",
        &[
            "/bin/sh",
            "-c",
            "ping -c 1 -W 2 10.199.99.1 || exit 44; \
             ping -c 1 -W 1 10.88.77.66 && exit 45; \
             exit 0",
        ],
    )
    .status()
    .expect("run supervise");
    assert_eq!(
        status.code(),
        Some(0),
        "44 = configured endpoint unreachable, 45 = unconfigured address reachable"
    );

    // Exiting destroyed the namespace, which destroyed the veth peer — the
    // host-side interface must be gone (teardown also deletes it explicitly).
    let leftover = Command::new("ip")
        .args(["link", "show", "acpstest0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("check leftover veth");
    assert!(
        !leftover.success(),
        "the host-side veth must not survive the spawn"
    );
}
