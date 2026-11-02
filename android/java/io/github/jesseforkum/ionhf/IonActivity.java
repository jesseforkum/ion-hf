package io.github.jesseforkum.ionhf;

import android.Manifest;
import android.app.NativeActivity;
import android.app.PendingIntent;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.hardware.usb.UsbDevice;
import android.hardware.usb.UsbDeviceConnection;
import android.hardware.usb.UsbManager;
import android.os.Build;
import android.os.Bundle;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/**
 * The Rust app (NativeActivity), the service that keeps it running in the background, and what
 * the Rust side calls over JNI: USB serial ports and a restart.
 */
public class IonActivity extends NativeActivity {
    private static final String ACTION_USB_PERMISSION = "io.github.jesseforkum.ionhf.USB";

    private volatile String serialError = "";
    private boolean usbAsked;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        List<String> ask = new ArrayList<>();
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            ask.add(Manifest.permission.RECORD_AUDIO);
        }
        if (Build.VERSION.SDK_INT >= 33) {
            ask.add(Manifest.permission.POST_NOTIFICATIONS);
        }
        if (!ask.isEmpty()) {
            requestPermissions(ask.toArray(new String[0]), 1);
        }
        startForegroundService(new Intent(this, IonService.class));
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] permissions, int[] results) {
        super.onRequestPermissionsResult(code, permissions, results);
        // With the microphone allowed, the service may record in the background too.
        startForegroundService(new Intent(this, IonService.class));
    }

    @Override
    protected void onDestroy() {
        if (isFinishing()) {
            stopService(new Intent(this, IonService.class));
        }
        super.onDestroy();
    }

    /**
     * Opens a USB serial bridge: {@code usb} (or empty) for the first CP210x, {@code usb:N} for its
     * interface N, else a device path. Null on failure, with {@link #serialError} saying why.
     */
    public UsbSerial openSerial(String name, int baud, int stopBits) {
        try {
            UsbManager um = getSystemService(UsbManager.class);
            String path = name;
            int index = 0;
            if (name.isEmpty() || name.startsWith("usb")) {
                path = null;
                int colon = name.indexOf(':');
                if (colon >= 0) {
                    index = Integer.parseInt(name.substring(colon + 1).trim());
                }
            }
            UsbDevice dev = null;
            for (UsbDevice d : um.getDeviceList().values()) {
                boolean match = path != null
                        ? d.getDeviceName().equals(path)
                        : d.getVendorId() == UsbSerial.VENDOR_SILABS;
                if (match) {
                    dev = d;
                    break;
                }
            }
            if (dev == null) {
                serialError = "no USB serial bridge";
                return null;
            }
            if (!um.hasPermission(dev)) {
                // Once a run: the rig thread retries, and the dialog should not.
                if (!usbAsked) {
                    usbAsked = true;
                    Intent i = new Intent(ACTION_USB_PERMISSION).setPackage(getPackageName());
                    um.requestPermission(
                            dev, PendingIntent.getBroadcast(this, 0, i, PendingIntent.FLAG_IMMUTABLE));
                }
                serialError = "allow USB access";
                return null;
            }
            if (index >= dev.getInterfaceCount()) {
                serialError = "no interface " + index;
                return null;
            }
            UsbDeviceConnection conn = um.openDevice(dev);
            if (conn == null) {
                serialError = "cannot open";
                return null;
            }
            try {
                return new UsbSerial(conn, dev.getInterface(index), baud, stopBits);
            } catch (IOException e) {
                conn.close();
                serialError = e.getMessage();
                return null;
            }
        } catch (RuntimeException e) {
            serialError = String.valueOf(e.getMessage());
            return null;
        }
    }

    public String serialError() {
        return serialError;
    }

    /** Closes the app and starts it again: radio settings take effect at start. */
    public void restart() {
        // No lambdas: javac against android.jar lacks their bootstrap.
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Intent i = getPackageManager().getLaunchIntentForPackage(getPackageName());
                i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK);
                stopService(new Intent(IonActivity.this, IonService.class));
                startActivity(i);
                Runtime.getRuntime().exit(0);
            }
        });
    }
}
