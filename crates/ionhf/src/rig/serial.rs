//! A COM port (a USB serial bridge on Windows, `/dev/tty*` elsewhere).

use std::io::{Read, Write};
use std::time::Duration;

use super::{Line, Port, RigError, RigResult};

pub struct SerialPort {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialPort {
    /// Opens `name` 8-N-`stop_bits` with no flow control, then drops RTS
    /// and DTR at once: some radios are set to key on either, and Windows
    /// drivers can raise DTR on open.
    pub fn open(name: &str, baud: u32, stop_bits: u8) -> RigResult<SerialPort> {
        let stop = if stop_bits == 2 {
            serialport::StopBits::Two
        } else {
            serialport::StopBits::One
        };
        let mut port = serialport::new(name, baud)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(stop)
            .flow_control(serialport::FlowControl::None)
            .dtr_on_open(false)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|e| match e.kind() {
                serialport::ErrorKind::NoDevice => {
                    RigError::Link(format!("{name}: no such port, or in use by another program"))
                }
                _ => RigError::Link(format!("{name}: {e}")),
            })?;
        let _ = port.write_request_to_send(false);
        let _ = port.write_data_terminal_ready(false);
        Ok(SerialPort { port })
    }
}

impl Read for SerialPort {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.port.read(buf)
    }
}

impl Write for SerialPort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.port.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.port.flush()
    }
}

impl Port for SerialPort {
    fn set_line(&mut self, line: Line, on: bool) -> std::io::Result<()> {
        match line {
            Line::Rts => self.port.write_request_to_send(on),
            Line::Dtr => self.port.write_data_terminal_ready(on),
        }
        .map_err(std::io::Error::other)
    }
    fn clear_input(&mut self) -> std::io::Result<()> {
        self.port
            .clear(serialport::ClearBuffer::Input)
            .map_err(std::io::Error::other)
    }
}

impl Drop for SerialPort {
    fn drop(&mut self) {
        // Never leave a line up that might be keying the radio.
        let _ = self.port.write_request_to_send(false);
        let _ = self.port.write_data_terminal_ready(false);
    }
}
