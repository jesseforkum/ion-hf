package io.github.jesseforkum.ionhf;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.Manifest;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;

/**
 * Keeps the process, and so the modem, alive while another app is in front or the screen is
 * off: host programs on this phone (APRSdroid over KISS, a Ragtime client) keep their link.
 */
public class IonService extends Service {
    private static final String CHANNEL = "ion";
    private PowerManager.WakeLock wake;
    private Notification note;

    @Override
    public void onCreate() {
        super.onCreate();
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(
                new NotificationChannel(CHANNEL, "Modem", NotificationManager.IMPORTANCE_LOW));
        PendingIntent open = PendingIntent.getActivity(
                this, 0, new Intent(this, IonActivity.class), PendingIntent.FLAG_IMMUTABLE);
        note = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(getApplicationInfo().icon)
                .setContentTitle("Ion HF")
                .setContentText("Running")
                .setContentIntent(open)
                .setOngoing(true)
                .build();
        promote();
        PowerManager pm = getSystemService(PowerManager.class);
        wake = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "ionhf:modem");
        wake.acquire();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        promote();
        return START_NOT_STICKY;
    }

    /** Foreground, recording too once the microphone is allowed (Android 14 asks both). */
    private void promote() {
        if (Build.VERSION.SDK_INT < 29) {
            startForeground(1, note);
            return;
        }
        int type = ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE;
        if (Build.VERSION.SDK_INT >= 30
                && checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
            type |= ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE;
        }
        startForeground(1, note, type);
    }

    @Override
    public void onDestroy() {
        if (wake != null && wake.isHeld()) {
            wake.release();
        }
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
