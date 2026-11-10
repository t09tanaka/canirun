use canirun::error::Result;
use canirun::{managed, Context};

/// Settle a ledger entry whose owner is gone, after checking that nothing it
/// started is still running. Exit 0 when released, 1 when kept, 3 when the
/// ledger has no such entry (2 stays "error", as everywhere else).
pub fn run(ctx: &Context, id: &str, json: bool, force: bool) -> Result<i32> {
    let r = managed::resolve_checked_with(ctx, id, force)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
        if !r.found {
            return Ok(3);
        }
    } else {
        println!("{}", r.command_id);
        if !r.found {
            println!("  result:  no such entry; there is nothing to release");
            return Ok(3);
        }
        match r.phase {
            Some(p) => println!("  state:   {p:?}, owner lease {}", held(r.lease_held)),
            None => println!(
                "  state:   no readable sidecar, owner lease {}",
                held(r.lease_held)
            ),
        }
        println!("  checked: {}", r.checked);
        if r.forced {
            println!("  result:  released by --force; recorded as interrupted");
        } else if r.released {
            println!("  result:  released; recorded as interrupted");
        } else {
            println!("  result:  kept, still charged. Run again once the cause is gone");
        }
    }
    Ok(if r.released { 0 } else { 1 })
}

fn held(held: bool) -> &'static str {
    if held {
        "held"
    } else {
        "gone"
    }
}
