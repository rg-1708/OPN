//! Command/event coverage, compiler-enforced (roadmap cross-cutting rule 2):
//! adding a `Cmd` or `Evt` variant without naming its covering integration
//! test breaks this build. The names are strings (tests live in other
//! binaries), but the *exhaustive match* is the enforcement — a new variant
//! fails to compile until someone points at a test here, and pointing at a
//! test you didn't write is a lie reviewers can see.

use contracts::{Cmd, Evt};

#[test]
fn every_cmd_names_its_covering_test() {
    // Signature exists so the match must be total over Cmd; never called.
    fn covering_test(cmd: &Cmd) -> &'static str {
        match cmd {
            Cmd::Auth { .. } => "ws::auth_happy_path (+ bad_jwt_closes_4401)",
            Cmd::Sub { .. } => "ws::sub_authz, ws::presence_snapshot_and_transitions",
            Cmd::Unsub { .. } => "ws::sub_authz",
            Cmd::AuthRefresh => "ws::auth_refresh_returns_fresh_token",
            Cmd::IdentityMe => "ws::auth_happy_path, identity::me_*",
            Cmd::IdentityAppLogin { .. } => "identity::app_login_*",
            Cmd::IdentityGetSettings { .. } => "identity::settings_roundtrip",
            Cmd::IdentitySetSettings { .. } => "identity::settings_roundtrip",
            Cmd::IdentitySetSharePresence { .. } => "ws::share_presence_off_null_snapshot",
            Cmd::ChannelsSend { .. } => {
                "channels_seq::concurrent_senders_gapless, channels::send_delivers_to_subscriber"
            }
            Cmd::ChannelsOpenDirect { .. } => "channels::open_direct_found_or_create",
            Cmd::ChannelsCreate { .. } => "channels::create_group_and_list",
            Cmd::ChannelsList => "channels::create_group_and_list",
            Cmd::ChannelsMarkDelivered { .. } => {
                "channels_receipts::receipts_both_kinds, ::receipts_monotonic_and_emit"
            }
            Cmd::ChannelsMarkRead { .. } => {
                "channels_receipts::receipts_both_kinds, ::receipts_monotonic_and_emit"
            }
            Cmd::ChannelsTyping { .. } => "channels_receipts::typing_delivered_and_authz",
            Cmd::ChannelsReact { .. } => "channels_reactions_pins::react_add_remove_and_authz",
            Cmd::ChannelsUnreact { .. } => "channels_reactions_pins::react_add_remove_and_authz",
            Cmd::ChannelsPin { .. } => "channels_reactions_pins::pins_cap_50",
            Cmd::ChannelsUnpin { .. } => "channels_reactions_pins::pin_unpin_roundtrip",
            Cmd::ChannelsMemberAdd { .. } => {
                "channels_members_resume::member_add_remove_group_only"
            }
            Cmd::ChannelsMemberRemove { .. } => {
                "channels_members_resume::member_remove_drops_subscription"
            }
            Cmd::NotifySeen { .. } => "notify::seen_marks_rows",
            Cmd::NotifyClear => "notify::clear_empties_inbox",
        }
    }
    let _ = covering_test;
}

#[test]
fn every_evt_names_its_covering_test() {
    fn covering_test(evt: &Evt) -> &'static str {
        match evt {
            Evt::PresenceState { .. } => "ws::presence_snapshot_and_transitions",
            Evt::ChannelsMessage { .. } => "channels::send_delivers_to_subscriber",
            Evt::NotifyEvent { .. } => "notify::route_pushes_to_online_recipient",
            Evt::ChannelsReceipt { .. } => "channels_receipts::receipts_monotonic_and_emit",
            Evt::ChannelsTyping { .. } => "channels_receipts::typing_delivered_and_authz",
            Evt::ChannelsReaction { .. } => "channels_reactions_pins::react_add_remove_and_authz",
            Evt::ChannelsPin { .. } => "channels_reactions_pins::pins_cap_50",
            Evt::ChannelsMember { .. } => "channels_members_resume::member_add_remove_group_only",
            Evt::ChannelsResumeOverflow { .. } => "channels_members_resume::resume_overflow_at_cap",
        }
    }
    let _ = covering_test;
}
