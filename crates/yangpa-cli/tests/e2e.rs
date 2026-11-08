//! 가짜 데몬(임시 소켓의 RPC 서버)에 CLI 명령을 실행해, 이름이 ID로 바뀌고 기획서 예시가 올바른 인자로 가는지 봅니다.

use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use serde_json::{Value, json};
use std::sync::Mutex;
use yangpa_core::device::Profile;
use yangpa_core::ids::{ChannelId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::model::{ChannelSettings, Permissions, Role, SpaceKind};
use yangpa_core::time::Timestamp;
use yangpa_rpc::api::{
    BanView, BlockedView, ChannelView, MemberView, PendingConfirmation, Scope, SpaceSummary,
};
use yangpa_rpc::server::{AuthInfo, ConnCtx, EventHub, RpcHandler, RpcOutput, ServerHandle};
use yangpa_rpc::{Endpoint, RpcError};

use yangpa_cli::args::Cli;
use yangpa_cli::{EXIT_CONFIRMATION, EXIT_ERROR};

const CREW: SpaceId = SpaceId([1; 16]);
const NOTICE: ChannelId = ChannelId([2; 16]);
const STAFF: RoleId = RoleId([3; 16]);
const CHULSU: UserId = UserId([11; 32]);
const YOUNGHEE: UserId = UserId([12; 32]);
const MINSU: UserId = UserId([13; 32]);

fn named(name: &str) -> Profile {
    Profile {
        name: name.into(),
        ..Profile::default()
    }
}

#[derive(Default)]
struct Daemon {
    calls: Mutex<Vec<(String, Value)>>,
}

impl Daemon {
    fn last(&self, method: &str) -> Value {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| panic!("{method} 호출 없음"))
    }
}

#[async_trait]
impl RpcHandler for Daemon {
    async fn authenticate(&self, token: &str) -> Option<AuthInfo> {
        (token == "owner-token").then(|| AuthInfo {
            token_id: "owner".into(),
            token_name: "owner".into(),
            scopes: vec![Scope::Owner],
            spaces: vec![],
            automation: false,
        })
    }

    async fn handle(
        &self,
        _ctx: &ConnCtx,
        method: &str,
        params: Value,
    ) -> Result<RpcOutput, RpcError> {
        let dismiss = params["action"] == "dismiss";
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        if method == "blob.read" {
            return Ok(RpcOutput::Binary(b"secret".to_vec().into()));
        }
        let v = match method {
            "space.list" => serde_json::to_value(vec![SpaceSummary {
                space: CREW,
                kind: SpaceKind::Crew,
                name: "게임 모임".into(),
                icon: None,
                unread: 0,
                mention: 0,
                dormant: false,
                member_count: 3,
            }])
            .map_err(|e| RpcError::internal(e.to_string()))?,
            "channel.list" => serde_json::to_value(vec![ChannelView {
                channel: NOTICE,
                settings: ChannelSettings::text("공지"),
                unread: 0,
                joined: None,
                participants: None,
                api_read_allowed: false,
                p2p: false,
                p2p_joined: false,
                anonymous: false,
                featured_post: None,
                attach_limit: None,
                no_edit: false,
                no_delete: false,
                volatile_cap: None,
                volatile_full: false,
                has_new: false,
            }])
            .map_err(|e| RpcError::internal(e.to_string()))?,
            "role.list" => serde_json::to_value(vec![
                Role::everyone(Permissions::DEFAULT_EVERYONE),
                Role {
                    id: STAFF,
                    name: "운영진".into(),
                    color: 0,
                    rank: 10,
                    permissions: Permissions::MANAGE_CHANNELS,
                    automation: false,
                },
            ])
            .map_err(|e| RpcError::internal(e.to_string()))?,
            "crew.create" => json!({ "space": CREW.to_string() }),
            "channel.create" => json!({ "channel": ChannelId([9; 16]).to_string() }),
            "role.create" => json!({ "role": RoleId([8; 16]).to_string() }),
            "invite.create" => json!({ "url": "yangpa://invite/abc" }),
            "message.send" => json!({ "id": yangpa_core::ids::EventId([7; 32]).to_string() }),
            "member.kick" => return Err(RpcError::confirmation_required("c-7")),
            "member.list" => serde_json::to_value(vec![MemberView {
                user: CHULSU,
                profile: named("철수"),
                nickname: None,
                roles: vec![],
                online: false,
                automation: false,
                away: false,
                blocked: false,
                last_seen: None,
            }])
            .map_err(|e| RpcError::internal(e.to_string()))?,
            "member.bans" => serde_json::to_value(vec![BanView {
                user: YOUNGHEE,
                profile: Some(named("영희")),
                reason: None,
                banned_by: CHULSU,
                banned_at: Timestamp(1),
            }])
            .map_err(|e| RpcError::internal(e.to_string()))?,
            "user.blocked" => serde_json::to_value(vec![BlockedView {
                user: MINSU,
                profile: Some(named("민수")),
                blocked_at: Timestamp(1),
            }])
            .map_err(|e| RpcError::internal(e.to_string()))?,
            "message.report" => json!({ "report_id": "r-1" }),
            "report.list" => json!([]),
            // 자동화 토큰이 dismiss 말고 다른 처리를 하면 데몬이 사람 확인을 요구합니다.
            "report.resolve" if !dismiss => {
                return Err(RpcError::confirmation_required("c-8"));
            }
            "confirm.list" => serde_json::to_value(vec![PendingConfirmation {
                id: "c-7".into(),
                method: "member.kick".into(),
                params: json!({ "space": CREW.to_string() }),
                token_name: "ai".into(),
                space: Some(CREW),
                requested_at: yangpa_core::time::Timestamp(1),
                expires_at: None,
                summary: "철수 추방".into(),
            }])
            .map_err(|e| RpcError::internal(e.to_string()))?,
            "confirm.approve" => json!({ "result": {} }),
            // 시험의 블롭은 모두 휘발성 채널 첨부입니다.
            "blob.status" => json!({
                "have_chunks": 1, "total_chunks": 1, "seeders_online": 0, "volatile": true
            }),
            _ => json!({}),
        };
        Ok(RpcOutput::Json(v))
    }
}

async fn start() -> (tempfile::TempDir, ServerHandle, Arc<Daemon>, String) {
    let dir = tempfile::tempdir().unwrap();
    let ep = Endpoint::in_dir(dir.path());
    let daemon = Arc::new(Daemon::default());
    let handle = yangpa_rpc::serve(ep.clone(), daemon.clone(), EventHub::new())
        .await
        .unwrap();
    (
        dir,
        handle,
        daemon,
        ep.path().to_string_lossy().into_owned(),
    )
}

async fn yangpa(ep: &str, args: &[&str]) -> i32 {
    let mut v = vec![
        "yangpa",
        "--endpoint",
        ep,
        "--token",
        "owner-token",
        "--json",
    ];
    v.extend_from_slice(args);
    yangpa_cli::run(Cli::try_parse_from(v).unwrap()).await
}

#[tokio::test]
async fn plan_examples_reach_daemon_with_ids() {
    let (_dir, _h, d, ep) = start().await;

    assert_eq!(yangpa(&ep, &["crew", "create", "게임 모임"]).await, 0);
    assert_eq!(d.last("crew.create")["name"], "게임 모임");

    assert_eq!(
        yangpa(
            &ep,
            &[
                "channel",
                "create",
                "--crew",
                "게임 모임",
                "--type",
                "text",
                "--name",
                "비밀",
                "--volatile",
                "--join-leave",
                "--ttl",
                "30s"
            ]
        )
        .await,
        0
    );
    let p = d.last("channel.create");
    assert_eq!(p["space"], CREW.to_string());
    assert_eq!(p["kind"], "text");
    assert_eq!(p["name"], "비밀");
    assert_eq!(p["security"], "volatile");
    assert_eq!(p["join_mode"], "join_leave");
    assert_eq!(p["ttl_secs"], 30);

    assert_eq!(
        yangpa(
            &ep,
            &[
                "role",
                "create",
                "--crew",
                "게임 모임",
                "--name",
                "운영진",
                "--perms",
                "manage_channels,kick"
            ]
        )
        .await,
        0
    );
    assert_eq!(
        d.last("role.create")["permissions"],
        json!(["manage_channels", "kick"])
    );

    assert_eq!(
        yangpa(
            &ep,
            &[
                "invite",
                "create",
                "--crew",
                "게임 모임",
                "--expires",
                "24h",
                "--uses",
                "10",
                "--roles",
                "운영진"
            ]
        )
        .await,
        0
    );
    let p = d.last("invite.create");
    assert_eq!(p["expires_in_secs"], 86_400);
    assert_eq!(p["max_uses"], 10);
    assert_eq!(p["roles"], json!([STAFF.to_string()]));

    assert_eq!(
        yangpa(
            &ep,
            &[
                "seed",
                "set",
                "--crew",
                "게임 모임",
                "--types",
                "text,emoji",
                "--upload-limit",
                "2MB/s",
                "--exclude-channels",
                "#공지"
            ]
        )
        .await,
        0
    );
    let p = d.last("seed.set");
    assert_eq!(p["types"], json!(["text", "emoji"]));
    assert_eq!(p["upload_limit_bps"], 2 * 1024 * 1024);
    assert_eq!(p["exclude_channels"], json!([NOTICE.to_string()]));
    assert!(
        p.get("channels").is_none(),
        "채널 범위는 건드리지 않아야 함: {p}"
    );

    // 무제한은 null로 보내 상한을 지웁니다.
    assert_eq!(
        yangpa(&ep, &["seed", "set", "--upload-limit", "unlimited"]).await,
        0
    );
    let p = d.last("seed.set");
    assert!(p.get("upload_limit_bps").is_some_and(Value::is_null), "{p}");
    assert!(p.get("space").is_none_or(Value::is_null));
}

#[tokio::test]
async fn update_and_message_commands() {
    let (_dir, _h, d, ep) = start().await;
    assert_eq!(
        yangpa(
            &ep,
            &[
                "channel",
                "update",
                "--crew",
                "게임 모임",
                "#공지",
                "--ttl",
                "none",
                "--public",
                "--no-parent"
            ]
        )
        .await,
        0
    );
    let p = d.last("channel.update");
    assert_eq!(p["channel"], NOTICE.to_string());
    assert_eq!(p["ttl_secs"], 0);
    assert_eq!(p["private_roles"], json!([]));
    assert!(p["parent"].is_null() && p.get("parent").is_some());

    assert_eq!(
        yangpa(
            &ep,
            &[
                "message",
                "send",
                "--crew",
                "게임 모임",
                "-c",
                "공지",
                "안녕하세요",
                "-a",
                "사진.png"
            ]
        )
        .await,
        0
    );
    let p = d.last("message.send");
    assert_eq!(p["text"], "안녕하세요");
    let path = p["attachments"][0]["path"].as_str().unwrap();
    assert!(std::path::Path::new(path).is_absolute(), "{path}");
}

#[tokio::test]
async fn errors_map_to_exit_codes() {
    let (_dir, _h, d, ep) = start().await;
    // 사람 확인 대기는 3.
    let user = yangpa_core::ids::UserId([5; 32]).to_string();
    assert_eq!(
        yangpa(&ep, &["member", "kick", "--crew", "게임 모임", &user]).await,
        EXIT_CONFIRMATION
    );
    // 없는 크루 이름은 데몬에 보내지 않고 실패.
    let before = d.calls.lock().unwrap().len();
    assert_eq!(
        yangpa(&ep, &["channel", "list", "--crew", "없는 모임"]).await,
        EXIT_ERROR
    );
    {
        let calls = d.calls.lock().unwrap();
        assert!(calls[before..].iter().all(|(m, _)| m == "space.list"));
    }
    // 잘못된 토큰.
    let v = vec![
        "yangpa",
        "--endpoint",
        &ep,
        "--token",
        "wrong",
        "space",
        "list",
    ];
    assert_eq!(
        yangpa_cli::run(Cli::try_parse_from(v).unwrap()).await,
        EXIT_ERROR
    );
    // 범위를 바꾸지 않는 seed set은 사용법 오류로 끊습니다(1).
    assert_eq!(yangpa(&ep, &["seed", "set"]).await, EXIT_ERROR);
}

#[tokio::test]
async fn confirm_approve_checks_the_pending_item() {
    let (_dir, _h, d, ep) = start().await;
    let approvals = |d: &Daemon| {
        d.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "confirm.approve")
            .count()
    };
    // 목록에 없는 ID는 승인 요청을 보내지 않습니다(오타·엉뚱한 ID로 다른 작업을 승인하지 않게).
    assert_eq!(
        yangpa(&ep, &["confirm", "approve", "c-999", "--yes"]).await,
        EXIT_ERROR
    );
    assert_eq!(approvals(&d), 0);
    // 목록에 있으면 내용을 보여 주고 승인합니다.
    assert_eq!(
        yangpa(&ep, &["confirm", "approve", "c-7", "--yes"]).await,
        0
    );
    assert_eq!(approvals(&d), 1);
    assert_eq!(d.last("confirm.approve")["id"], "c-7");
}

#[tokio::test]
async fn member_roles_resolves_names() {
    let (_dir, _h, d, ep) = start().await;
    let user = yangpa_core::ids::UserId([5; 32]).to_string();
    assert_eq!(
        yangpa(
            &ep,
            &[
                "member",
                "roles",
                "--crew",
                "게임 모임",
                &user,
                "--roles",
                "운영진"
            ]
        )
        .await,
        0
    );
    assert_eq!(
        d.last("member.set_roles")["roles"],
        json!([STAFF.to_string()])
    );
    assert_eq!(
        yangpa(
            &ep,
            &[
                "member",
                "roles",
                "--crew",
                "게임 모임",
                &user,
                "--roles",
                ""
            ]
        )
        .await,
        0
    );
    assert_eq!(d.last("member.set_roles")["roles"], json!([]));
}

#[tokio::test]
async fn moderation_commands_resolve_names() {
    let (_dir, _h, d, ep) = start().await;
    let calls = |d: &Daemon, method: &str| {
        d.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .count()
    };

    // 밴은 멤버 이름으로, 밴 해제는 (멤버 목록에 없는) 밴 목록 이름으로 찾습니다.
    assert_eq!(
        yangpa(
            &ep,
            &[
                "member",
                "ban",
                "--crew",
                "게임 모임",
                "철수",
                "--reason",
                "도배",
                "--delete-recent"
            ]
        )
        .await,
        0
    );
    let p = d.last("member.ban");
    assert_eq!(p["space"], CREW.to_string());
    assert_eq!(p["user"], CHULSU.to_string());
    assert_eq!(p["reason"], "도배");
    assert_eq!(p["delete_recent"], true);
    assert_eq!(
        yangpa(&ep, &["member", "ban", "--crew", "게임 모임", "@철수"]).await,
        0
    );
    assert!(d.last("member.ban").get("delete_recent").is_none());
    assert_eq!(
        yangpa(&ep, &["member", "unban", "--crew", "게임 모임", "영희"]).await,
        0
    );
    assert_eq!(d.last("member.unban")["user"], YOUNGHEE.to_string());
    assert_eq!(
        yangpa(&ep, &["member", "bans", "--crew", "게임 모임"]).await,
        0
    );
    assert_eq!(d.last("member.bans")["space"], CREW.to_string());

    // 차단: --crew면 그 크루 멤버 이름, 해제는 차단 목록 이름.
    assert_eq!(
        yangpa(&ep, &["user", "block", "--crew", "게임 모임", "철수"]).await,
        0
    );
    assert_eq!(d.last("user.block")["user"], CHULSU.to_string());
    assert_eq!(yangpa(&ep, &["user", "unblock", "민수"]).await, 0);
    assert_eq!(d.last("user.unblock")["user"], MINSU.to_string());
    assert_eq!(yangpa(&ep, &["user", "blocked"]).await, 0);
    assert_eq!(calls(&d, "user.blocked"), 2);
    // 없는 이름은 데몬에 차단 요청을 보내지 않습니다.
    assert_eq!(
        yangpa(&ep, &["user", "unblock", "없는 사람"]).await,
        EXIT_ERROR
    );
    assert_eq!(calls(&d, "user.unblock"), 1);

    // 신고.
    let msg = EventId([7; 32]).to_string();
    assert_eq!(
        yangpa(
            &ep,
            &[
                "message",
                "report",
                "--crew",
                "게임 모임",
                "-c",
                "#공지",
                &msg,
                "--reason",
                "스팸",
                "--hide"
            ]
        )
        .await,
        0
    );
    let p = d.last("message.report");
    assert_eq!(p["channel"], NOTICE.to_string());
    assert_eq!(p["message"], msg);
    assert_eq!(p["reason"], "스팸");
    assert_eq!(p["hide"], true);
    // 빈 이유는 보내기 전에 끊습니다.
    assert_eq!(
        yangpa(
            &ep,
            &[
                "message",
                "report",
                "--crew",
                "게임 모임",
                "-c",
                "공지",
                &msg,
                "--reason",
                "  "
            ]
        )
        .await,
        EXIT_ERROR
    );
    assert_eq!(calls(&d, "message.report"), 1);

    // 신고 목록·처리.
    assert_eq!(
        yangpa(
            &ep,
            &["reports", "ls", "--crew", "게임 모임", "--status", "open"]
        )
        .await,
        0
    );
    assert_eq!(d.last("report.list")["status"], "open");
    assert_eq!(
        yangpa(
            &ep,
            &[
                "report",
                "resolve",
                "--crew",
                "게임 모임",
                "r-1",
                "--action",
                "delete_message",
                "--note",
                "확인함"
            ]
        )
        .await,
        EXIT_CONFIRMATION
    );
    let p = d.last("report.resolve");
    assert_eq!(p["report_id"], "r-1");
    assert_eq!(p["action"], "delete_message");
    assert_eq!(p["note"], "확인함");
    assert_eq!(
        yangpa(
            &ep,
            &[
                "report",
                "resolve",
                "--crew",
                "게임 모임",
                "r-1",
                "--action",
                "dismiss"
            ]
        )
        .await,
        0
    );
    assert_eq!(d.last("report.resolve")["action"], "dismiss");
}

#[tokio::test]
async fn blob_read_asks_before_writing_volatile_to_a_file() {
    let (dir, _h, _d, ep) = start().await;
    let blob = yangpa_core::ids::BlobId([21; 32]).to_string();
    let key = EventId([7; 32]).to_string();
    let out = dir.path().join("첨부.bin");
    let out = out.to_str().unwrap();
    let read = |extra: &'static [&'static str], to: &str| {
        let mut v = vec![
            "blob",
            "read",
            "--crew",
            "게임 모임",
            &blob,
            "--key-from",
            &key,
            "-o",
        ];
        v.push(to);
        v.extend_from_slice(extra);
        v.into_iter().map(str::to_owned).collect::<Vec<_>>()
    };
    let run = |args: Vec<String>| {
        let ep = ep.clone();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            yangpa(&ep, &args).await
        }
    };
    // 파일로는 확인(--allow-volatile) 없이 쓰지 않고, 표준 출력은 그대로 됩니다.
    assert_eq!(run(read(&[], out)).await, EXIT_ERROR);
    assert!(!std::path::Path::new(out).exists());
    assert_eq!(run(read(&[], "-")).await, 0);
    assert_eq!(run(read(&["--allow-volatile"], out)).await, 0);
    assert_eq!(std::fs::read(out).unwrap(), b"secret");
}
