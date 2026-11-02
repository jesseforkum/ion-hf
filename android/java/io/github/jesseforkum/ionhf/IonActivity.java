package io.github.jesseforkum.ionhf;

import android.Manifest;
import android.app.NativeActivity;
import android.content.Intent;
import android.os.Build;
import android.os.Bundle;

/** The Rust app (NativeActivity), plus the service that keeps it running in the background. */
public class IonActivity extends NativeActivity {
    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        if (Build.VERSION.SDK_INT >= 33) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
        startForegroundService(new Intent(this, IonService.class));
    }

    @Override
    protected void onDestroy() {
        if (isFinishing()) {
            stopService(new Intent(this, IonService.class));
        }
        super.onDestroy();
    }
}
