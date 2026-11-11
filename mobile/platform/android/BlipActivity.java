package com.amov.blip.mobile;

import android.Manifest;
import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.PowerManager;
import android.provider.Settings;
import android.view.DisplayCutout;
import android.view.WindowInsets;

import dev.gpui.mobile.GpuiActivity;

/** Starts Blip's foreground service while the native activity is alive. */
public final class BlipActivity extends GpuiActivity {
    private static final int NOTIFICATION_PERMISSION_REQUEST = 9021;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        startForegroundService(new Intent(this, BlipBackgroundService.class));
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(
                    new String[] {Manifest.permission.POST_NOTIFICATIONS},
                    NOTIFICATION_PERMISSION_REQUEST);
        } else {
            requestBackgroundExemption();
        }
    }

    @Override
    public void onRequestPermissionsResult(
            int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        if (requestCode == NOTIFICATION_PERMISSION_REQUEST) {
            requestBackgroundExemption();
        }
    }

    /** Returns top, bottom, left and right safe-area insets in logical pixels. */
    @SuppressWarnings("deprecation")
    public static float[] getSafeAreaInsets(Activity activity) {
        WindowInsets windowInsets = activity.getWindow()
                .getDecorView()
                .getRootWindowInsets();
        if (windowInsets == null) return new float[] {0, 0, 0, 0};

        int top;
        int bottom;
        int left;
        int right;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            Insets system = windowInsets.getInsets(
                    WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
            Insets gestures = windowInsets.getInsets(WindowInsets.Type.mandatorySystemGestures());
            top = system.top;
            bottom = Math.max(system.bottom, gestures.bottom);
            left = system.left;
            right = system.right;
        } else {
            top = windowInsets.getSystemWindowInsetTop();
            bottom = windowInsets.getSystemWindowInsetBottom();
            left = windowInsets.getSystemWindowInsetLeft();
            right = windowInsets.getSystemWindowInsetRight();
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                DisplayCutout cutout = windowInsets.getDisplayCutout();
                if (cutout != null) {
                    top = Math.max(top, cutout.getSafeInsetTop());
                    bottom = Math.max(bottom, cutout.getSafeInsetBottom());
                    left = Math.max(left, cutout.getSafeInsetLeft());
                    right = Math.max(right, cutout.getSafeInsetRight());
                }
            }
        }

        float density = activity.getResources().getDisplayMetrics().density;
        return new float[] {
                top / density,
                bottom / density,
                left / density,
                right / density
        };
    }

    private void requestBackgroundExemption() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return;
        PowerManager power = (PowerManager) getSystemService(POWER_SERVICE);
        if (power == null || power.isIgnoringBatteryOptimizations(getPackageName())) return;

        Intent request = new Intent(
                Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                Uri.parse("package:" + getPackageName()));
        try {
            startActivity(request);
        } catch (ActivityNotFoundException missingDirectRequest) {
            try {
                startActivity(new Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS));
            } catch (ActivityNotFoundException ignored) {
                // Some vendor builds expose neither standard settings screen.
            }
        }
    }
}
