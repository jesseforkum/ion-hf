package io.github.jesseforkum.ionhf;

import android.hardware.usb.UsbConstants;
import android.hardware.usb.UsbDeviceConnection;
import android.hardware.usb.UsbEndpoint;
import android.hardware.usb.UsbInterface;
import java.io.IOException;

/**
 * A CP210x USB serial bridge, as in the Digirig and many radios: the vendor requests of
 * Silicon Labs AN571 ("CP210x Virtual COM Port Interface").
 */
public final class UsbSerial {
    static final int VENDOR_SILABS = 0x10C4;

    /** Host to device, vendor request, to an interface. */
    private static final int REQ_OUT = 0x41;
    private static final int IFC_ENABLE = 0x00;
    private static final int SET_LINE_CTL = 0x03;
    private static final int SET_MHS = 0x07;
    private static final int SET_BAUDRATE = 0x1E;
    private static final int TIMEOUT_MS = 500;

    private final UsbDeviceConnection conn;
    private final UsbInterface iface;
    private final UsbEndpoint in;
    private final UsbEndpoint out;
    private final byte[] spill = new byte[256];

    UsbSerial(UsbDeviceConnection conn, UsbInterface iface, int baud, int stopBits)
            throws IOException {
        this.conn = conn;
        this.iface = iface;
        UsbEndpoint i = null;
        UsbEndpoint o = null;
        for (int k = 0; k < iface.getEndpointCount(); k++) {
            UsbEndpoint e = iface.getEndpoint(k);
            if (e.getType() != UsbConstants.USB_ENDPOINT_XFER_BULK) {
                continue;
            }
            if (e.getDirection() == UsbConstants.USB_DIR_IN) {
                i = e;
            } else {
                o = e;
            }
        }
        if (i == null || o == null) {
            throw new IOException("not a serial interface");
        }
        in = i;
        out = o;
        if (!conn.claimInterface(iface, true)) {
            throw new IOException("in use");
        }
        control(IFC_ENABLE, 1, null);
        byte[] rate = {(byte) baud, (byte) (baud >> 8), (byte) (baud >> 16), (byte) (baud >> 24)};
        control(SET_BAUDRATE, 0, rate);
        // 8 data bits, no parity, 1 or 2 stop bits.
        control(SET_LINE_CTL, 0x0800 | (stopBits == 2 ? 2 : 0), null);
        if (!setLines(false, false)) {
            throw new IOException("control lines");
        }
    }

    private void control(int request, int value, byte[] data) throws IOException {
        int len = data == null ? 0 : data.length;
        if (conn.controlTransfer(REQ_OUT, request, value, iface.getId(), data, len, TIMEOUT_MS) < 0) {
            throw new IOException("control request " + request);
        }
    }

    /** DTR and RTS; false if the bridge refused. */
    public boolean setLines(boolean dtr, boolean rts) {
        int value = 0x0300 | (rts ? 2 : 0) | (dtr ? 1 : 0);
        return conn.controlTransfer(REQ_OUT, SET_MHS, value, iface.getId(), null, 0, TIMEOUT_MS) >= 0;
    }

    /** Bytes read into buf within timeoutMs; 0 or less for none. */
    public int read(byte[] buf, int timeoutMs) {
        return conn.bulkTransfer(in, buf, buf.length, timeoutMs);
    }

    public int write(byte[] buf) {
        return conn.bulkTransfer(out, buf, buf.length, TIMEOUT_MS);
    }

    /** Drops whatever is waiting to be read. */
    public void drain() {
        while (conn.bulkTransfer(in, spill, spill.length, 5) > 0) {
            // Discard.
        }
    }

    public void close() {
        setLines(false, false);
        conn.releaseInterface(iface);
        conn.close();
    }
}
