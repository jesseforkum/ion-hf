//! A USB serial bridge on Android (a CP210x, as in the Digirig): apps cannot
//! open `/dev/tty*`, so Java drives the bridge through the USB host API
//! (`UsbSerial.java`) and this calls it over JNI.

use std::io::{self, Read, Write};

use jni::objects::{GlobalRef, JString, JValue};

use super::{Line, Port, RigError, RigResult};
use crate::android::with_activity;

const READ_TIMEOUT_MS: i32 = 50;

pub struct SerialPort {
    usb: GlobalRef,
    rts: bool,
    dtr: bool,
}

impl SerialPort {
    /// `name`: `usb` (or empty) for the first CP210x bridge, `usb:1` for
    /// its second interface (a CP2105), else a USB device path. Opened
    /// 8-N-`stop_bits` with RTS and DTR down. The first try asks the user
    /// to allow the app the device, and fails until they do.
    pub fn open(name: &str, baud: u32, stop_bits: u8) -> RigResult<SerialPort> {
        let got = with_activity(|env, activity| {
            let jname = env.new_string(name)?;
            let usb = env
                .call_method(
                    activity,
                    "openSerial",
                    "(Ljava/lang/String;II)Lio/github/jesseforkum/ionhf/UsbSerial;",
                    &[
                        JValue::Object(&jname),
                        JValue::Int(baud as i32),
                        JValue::Int(stop_bits as i32),
                    ],
                )?
                .l()?;
            if usb.is_null() {
                let why = env
                    .call_method(activity, "serialError", "()Ljava/lang/String;", &[])?
                    .l()?;
                let why: String = env.get_string(&JString::from(why))?.into();
                return Ok(Err(why));
            }
            Ok(Ok(env.new_global_ref(usb)?))
        });
        match got {
            Some(Ok(usb)) => Ok(SerialPort {
                usb,
                rts: false,
                dtr: false,
            }),
            Some(Err(why)) => Err(RigError::Link(format!("{name}: {why}"))),
            None => Err(RigError::Link(format!("{name}: USB not available"))),
        }
    }

    fn send_lines(&self) -> io::Result<()> {
        let (dtr, rts) = (self.dtr as u8, self.rts as u8);
        let ok = with_activity(|env, _| {
            env.call_method(&self.usb, "setLines", "(ZZ)Z", &[JValue::Bool(dtr), JValue::Bool(rts)])?
                .z()
        });
        match ok {
            Some(true) => Ok(()),
            _ => Err(io::Error::other("USB control request failed")),
        }
    }
}

impl Read for SerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let got = with_activity(|env, _| {
            let arr = env.new_byte_array(buf.len() as i32)?;
            let n = env
                .call_method(
                    &self.usb,
                    "read",
                    "([BI)I",
                    &[JValue::Object(&arr), JValue::Int(READ_TIMEOUT_MS)],
                )?
                .i()?;
            if n > 0 {
                let mut got = vec![0i8; n as usize];
                env.get_byte_array_region(&arr, 0, &mut got)?;
                for (d, s) in buf.iter_mut().zip(got) {
                    *d = s as u8;
                }
            }
            Ok(n)
        });
        match got {
            Some(n) if n > 0 => Ok(n as usize),
            Some(_) => Err(io::ErrorKind::TimedOut.into()),
            None => Err(io::Error::other("USB read failed")),
        }
    }
}

impl Write for SerialPort {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let got = with_activity(|env, _| {
            let arr = env.byte_array_from_slice(buf)?;
            env.call_method(&self.usb, "write", "([B)I", &[JValue::Object(&arr)])?
                .i()
        });
        match got {
            Some(n) if n >= 0 => Ok(n as usize),
            _ => Err(io::Error::other("USB write failed")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Port for SerialPort {
    fn set_line(&mut self, line: Line, on: bool) -> io::Result<()> {
        match line {
            Line::Rts => self.rts = on,
            Line::Dtr => self.dtr = on,
        }
        self.send_lines()
    }

    fn clear_input(&mut self) -> io::Result<()> {
        with_activity(|env, _| env.call_method(&self.usb, "drain", "()V", &[]).map(|_| ()))
            .ok_or_else(|| io::Error::other("USB read failed"))
    }
}

impl Drop for SerialPort {
    fn drop(&mut self) {
        // Never leave a line up that might be keying the radio.
        with_activity(|env, _| env.call_method(&self.usb, "close", "()V", &[]).map(|_| ()));
    }
}
