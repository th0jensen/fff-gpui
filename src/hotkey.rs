use anyhow::{Result, anyhow};
use async_channel::Sender;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};

use crate::service::{CommandEnvelope, ServiceCommand};

pub fn install_event_handler(command_tx: Sender<CommandEnvelope>) {
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state() == HotKeyState::Pressed {
            let _ = command_tx.send_blocking((ServiceCommand::ToggleWindow, None));
        }
    }));
}

pub fn parse_hotkey(binding: Option<&str>) -> Result<Option<HotKey>> {
    let Some(binding) = binding else {
        return Ok(None);
    };

    let trimmed = binding.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if trimmed.split_whitespace().count() != 1 {
        return Err(anyhow!(
            "global hotkey must be a single shortcut, got {:?}",
            binding
        ));
    }

    let normalized = trimmed.replace('-', "+");
    let normalized = expand_hyper(&normalized);
    let hotkey = normalized
        .parse::<HotKey>()
        .map_err(|err| anyhow!("invalid global hotkey {:?}: {}", binding, err))?;
    Ok(Some(hotkey))
}

fn expand_hyper(binding: &str) -> String {
    let mut tokens = Vec::new();
    for token in binding.split('+') {
        let token = token.trim();
        if token.eq_ignore_ascii_case("hyper") {
            tokens.extend(["shift", "control", "alt", "super"]);
        } else if !token.is_empty() {
            tokens.push(token);
        }
    }
    tokens.join("+")
}

#[cfg(test)]
mod tests {
    use super::expand_hyper;

    #[test]
    fn expands_hyper_into_all_modifiers() {
        assert_eq!(expand_hyper("hyper+f"), "shift+control+alt+super+f");
        assert_eq!(expand_hyper("cmd-hyper-f"), "cmd+shift+control+alt+super+f");
    }
}
