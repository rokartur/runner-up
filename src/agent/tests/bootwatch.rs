use std::time::Duration;

use crate::agent::tart::watch_boot_session;
use crate::ctx::Ctx;

// Intent (README, "Macs", "A clone that restarts"): once a job runs in a tart
// clone, the agent reads the clone's boot session every guestCheckSeconds
// and ends the job when a new boot session shows the clone's macOS
// restarted, taking its runner with it. A reading that fails, or is no boot
// session, ends nothing, so a clone too busy to answer is never taken for a
// restarted one.
#[test]
fn only_a_new_boot_session_ends_the_job() {
    // kern.bootsessionuuid as a clone of macOS 15 prints it.
    const BOOT: &str = "6C1F5C0B-0A3E-4F3B-9E43-2D9C5D0B1A77";
    const REBOOT: &str = "0F9A2B4C-7D1E-4C2A-8B5F-3E6D7A8B9C0D";
    for (why, readings, want) in [
        ("the clone restarted", vec![BOOT, BOOT, REBOOT], true),
        ("too busy to answer at first", vec!["", "", BOOT, REBOOT], true),
        ("failed and odd readings", vec![BOOT, "", "sysctl: unknown oid 'kern.bootsessionuuid'", BOOT], false),
        ("the same boot throughout", vec![BOOT, BOOT, BOOT, BOOT], false),
    ] {
        let ctx = Ctx::background().with_cancel();
        let mut i = 0;
        let c = ctx.clone();
        let mut read = |_: &Ctx| {
            if i >= readings.len() {
                c.cancel();
                return String::new();
            }
            i += 1;
            readings[i - 1].to_string()
        };
        assert_eq!(watch_boot_session(&ctx, &mut read, Duration::from_millis(1)), want, "{why}: the watch said restarted");
        ctx.cancel();
    }
}
