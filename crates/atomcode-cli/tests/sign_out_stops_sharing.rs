//! 登出要先收共享,再删凭据。
//!
//! **为什么读源码而不是跑一次**:要让 `tui_share::sharing()` 在测试里为真,得起一个
//! hub、拉一条中继、走一次配对——那是在判「daemon 挂不挂得上」,不是在判「登出记不
//! 记得收它」。这条判据钉的是后者,而后者正是 2026-09-23 漏掉的那一行:共享
//! (`/webui`、`/sync`、`/app`)当天刚做完,登出没跟着收口,于是「我登出了」和
//! 「手机上还看得见这段对话」可以同时为真。
//!
//! 顺序也钉:删凭据那一步会失败(`?` 直接返回),排在它后面的收共享就有可能一次都
//! 不跑。共享是「谁能看到这个会话」,它必须先收。

use std::path::Path;

/// `host.rs` 里 `SignOut` 那一臂的正文。
fn sign_out_arm() -> String {
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/host.rs"))
        .expect("读 host.rs");
    let after = src
        .split_once("HostCommand::SignOut")
        .expect("host.rs 里没有 SignOut 这一臂了——这条判据要跟着改")
        .1;
    // 到下一臂为止。两个名字在这个文件里各只出现一次(它们就是那两臂的臂头)。
    after
        .split("HostCommand::SignIn")
        .next()
        .expect("SignOut 之后应当还有 SignIn 那一臂")
        .to_string()
}

#[test]
fn signing_out_stops_sharing_before_it_deletes_the_credentials() {
    let arm = sign_out_arm();
    let code: String = arm
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    let stopped = code.find("stop_all_sharing").expect(
        "登出没有收共享:凭据删了,而这个会话还挂在 hub 上、手机上还看得见。\
         见 tui_share::stop_all_sharing",
    );
    let logged_out = code
        .find("atomcode_auth::logout")
        .expect("SignOut 那一臂不删凭据了?这条判据要跟着改");

    assert!(
        stopped < logged_out,
        "收共享要排在删凭据之前:删凭据那一步会失败并直接返回,\
         排在它后面的收共享就可能一次都不跑"
    );
}
