//! Safety and packaging contract for the privileged QNAP service wrapper.

const SERVICE: &str = include_str!("../build/qnap/shared/unified-hifi-control.sh");
const UNINSTALL: &str = include_str!("../build/qnap/shared/uninstall.sh");
const QPKG_CONFIG: &str = include_str!("../build/qnap/qpkg.cfg");
const BUILD_WORKFLOW: &str = include_str!("../.github/workflows/build.yml");
const QNAP_DOCKERFILE: &str = include_str!("../build/qnap/Dockerfile");

#[test]
fn qpkg_allows_install_volume_selection_and_migration() {
    assert!(
        QPKG_CONFIG.contains("QPKG_VOLUME_SELECT=3"),
        "QNAP must allow selecting and migrating the package volume instead of forcing the system volume"
    );
}

#[test]
fn qnap_jobs_pin_qdk_architecture_to_the_binary_they_package() {
    let x64_job = BUILD_WORKFLOW
        .split("build-qnap-x64:")
        .nth(1)
        .and_then(|tail| tail.split("build-qnap-arm:").next())
        .expect("x64 QNAP job exists");
    let arm_job = BUILD_WORKFLOW
        .split("build-qnap-arm:")
        .nth(1)
        .and_then(|tail| tail.split("build-docker-x64:").next())
        .expect("ARM QNAP job exists");

    assert!(
        x64_job.contains("qbuild --build-dir /src/build --build-arch x86_64"),
        "the x64 package must be built for x86_64 explicitly"
    );
    assert!(
        arm_job.contains("qbuild --build-dir /src/build --build-arch arm_64"),
        "the ARM package must be built for arm_64 explicitly"
    );
}

#[test]
fn qnap_jobs_use_the_pinned_first_party_qdk_builder() {
    assert!(
        !BUILD_WORKFLOW.contains("owncloudci/qnap-qpkg-builder"),
        "QNAP builds must not depend on the unowned third-party builder image"
    );
    assert!(
        BUILD_WORKFLOW
            .matches("--file build/qnap/Dockerfile")
            .count()
            >= 2,
        "each QNAP architecture job must build the repository-owned QDK image"
    );
    assert!(
        BUILD_WORKFLOW.matches("qnap-qdk:2.5.3").count() >= 4,
        "both QNAP jobs must use the pinned QDK image tag for build and run"
    );
    assert!(
        QNAP_DOCKERFILE.contains("FROM ubuntu:20.04@sha256:"),
        "the QDK builder base image must be digest-pinned"
    );
    assert!(
        QNAP_DOCKERFILE.contains(
            "qnap-dev/QDK/releases/download/v${QDK_VERSION}/qdk_${QDK_VERSION}_amd64.deb"
        ),
        "the builder must download QDK from the official qnap-dev release"
    );
    assert!(
        QNAP_DOCKERFILE
            .contains("17b3841b7d4590a4ee025844ba583304b5e3c497d9fa8934d5175131d3908022"),
        "the official QDK 2.5.3 artifact must be checksum-pinned"
    );
    assert!(
        BUILD_WORKFLOW.contains("/usr/share/QDK/bin/qbuild"),
        "the workflow must invoke the path shipped by official QDK 2.5.3"
    );
}

#[test]
fn qnap_service_keeps_credentials_private_and_stops_only_its_recorded_process() {
    assert!(
        SERVICE.contains("umask 077"),
        "HQPlayer credentials and persistent backups must not inherit a permissive NAS umask"
    );
    assert!(
        !SERVICE.contains("PORT_PID="),
        "the package must never kill an unrelated process merely because it owns port 8088"
    );
    assert!(
        !SERVICE.contains("pkill -9"),
        "the package must not use a broad force-kill fallback"
    );
}

#[test]
fn qnap_service_refuses_a_stale_pid_reused_by_an_unrelated_process() {
    let stop = SERVICE
        .split("  stop)")
        .nth(1)
        .and_then(|tail| tail.split("  restart)").next())
        .expect("service wrapper has a stop arm");
    assert!(
        SERVICE.contains("readlink -f \"/proc/${PID_TO_CHECK}/exe\""),
        "a live numeric PID is insufficient; the wrapper must verify /proc identity"
    );

    let graceful = stop
        .find("kill \"$PID\"")
        .expect("stop arm sends a graceful signal");
    let first_identity = stop[..graceful]
        .rfind("is_our_pid \"$PID\"")
        .expect("identity is checked before the graceful signal");
    assert!(first_identity < graceful);

    let force = stop
        .find("kill -9 \"$PID\"")
        .expect("stop arm has a bounded force fallback");
    let second_identity = stop[..force]
        .rfind("is_our_pid \"$PID\"")
        .expect("identity is checked again before the force signal");
    assert!(
        second_identity > graceful,
        "PID identity must be rechecked after the grace period because the PID can be reused"
    );
}

#[test]
fn qnap_uninstall_refuses_a_stale_pid_reused_by_an_unrelated_process() {
    assert!(
        UNINSTALL.contains("readlink -f \"/proc/${PID_TO_CHECK}/exe\""),
        "uninstall must verify executable identity rather than trusting a live numeric PID"
    );

    let graceful = UNINSTALL
        .find("kill \"$PID\"")
        .expect("uninstall sends a graceful signal");
    let first_identity = UNINSTALL[..graceful]
        .rfind("is_our_pid \"$PID\"")
        .expect("uninstall checks identity before the graceful signal");
    assert!(first_identity < graceful);

    let force = UNINSTALL
        .find("kill -9 \"$PID\"")
        .expect("uninstall has a bounded force fallback");
    let second_identity = UNINSTALL[..force]
        .rfind("is_our_pid \"$PID\"")
        .expect("uninstall checks identity again before the force signal");
    assert!(
        second_identity > graceful,
        "uninstall must recheck identity after the grace period because the PID can be reused"
    );
}
