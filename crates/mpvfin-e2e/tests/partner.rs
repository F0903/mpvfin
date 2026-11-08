//! The scripted `SyncPlay` partner against the Docker Jellyfin.
//! Run: `cargo test -p mpvfin-e2e --test partner -- --ignored`

use std::time::Duration;

use mpvfin_e2e::{Partner, Server};

const T10: Duration = Duration::from_secs(10);

/// Waits for the next `StateUpdate` that reports `state`.
fn wait_state(partner: &Partner, state: &str) {
    loop {
        let update = partner.wait_update("StateUpdate", T10);
        if update["Data"]["State"] == state {
            return;
        }
    }
}

#[test]
#[ignore = "needs the Docker Jellyfin test server"]
fn two_partners_can_start_a_group() {
    let server = Server::connect();
    let episode = server.episodes()[0].0.clone();
    let first = Partner::connect(&server);
    let second = Partner::connect(&server);

    let group = first.create_group("partner-two-start");
    second.join(&group);
    second.wait_update("GroupJoined", T10);

    first.clear();
    second.clear();
    first.set_new_queue(&[&episode], 0, Duration::ZERO);
    wait_state(&first, "Playing");
    wait_state(&second, "Playing");
    for partner in [&first, &second] {
        assert_eq!(
            partner.wait_command("Unpause", T10)["PlaylistItemId"],
            partner.last_queue()["Playlist"][0]["PlaylistItemId"]
        );
    }
    first.leave();
    second.leave();
}
