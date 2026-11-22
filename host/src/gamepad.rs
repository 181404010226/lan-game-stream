use anyhow::{Context, Result};
use stream_wire::PadState;
use vigem_client::{Client, TargetId, XGamepad, Xbox360Wired};

pub struct Gamepad {
    target: Xbox360Wired<Client>,
    last: PadState,
}

impl Gamepad {
    pub fn connect() -> Result<Self> {
        let driver = Client::connect().context("电脑无法连接 ViGEmBus 虚拟手柄驱动")?;
        let mut target = Xbox360Wired::new(driver, TargetId::XBOX360_WIRED);
        target.plugin().context("无法创建 Xbox 360 手柄")?;
        target.wait_ready().context("Xbox 360 手柄未就绪")?;
        println!("Xbox 手柄已就绪");
        Ok(Self {
            target,
            last: PadState::default(),
        })
    }

    pub fn update(&mut self, state: PadState) -> Result<()> {
        if self.last == state {
            return Ok(());
        }
        let report = XGamepad {
            buttons: state.buttons.into(),
            thumb_lx: state.x,
            thumb_ly: state.y,
            ..XGamepad::default()
        };
        self.target
            .update(&report)
            .context("Xbox 手柄输入发送失败")?;
        self.last = state;
        if std::env::var_os("LAN_STREAM_TRACE_INPUT").is_some() {
            println!(
                "xinput buttons={:04x} lx={} ly={}",
                state.buttons, state.x, state.y
            );
        }
        Ok(())
    }
}

impl Drop for Gamepad {
    fn drop(&mut self) {
        let _ = self.update(PadState::default());
    }
}
