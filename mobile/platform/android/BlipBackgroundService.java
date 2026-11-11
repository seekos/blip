package com.amov.blip.mobile;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;

/** Keeps Blip's network connections and transfer workers alive in the background. */
public final class BlipBackgroundService extends Service {
    private static final String CHANNEL_ID = "blip-background";
    private static final int NOTIFICATION_ID = 9022;
    private WifiManager.MulticastLock multicastLock;
    private PowerManager.WakeLock wakeLock;

    @Override
    public void onCreate() {
        super.onCreate();
        PowerManager power = (PowerManager) getSystemService(Context.POWER_SERVICE);
        if (power != null) {
            wakeLock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "blip:sftp");
            wakeLock.setReferenceCounted(false);
            wakeLock.acquire();
        }

        WifiManager wifi = (WifiManager) getApplicationContext()
                .getSystemService(Context.WIFI_SERVICE);
        if (wifi != null) {
            multicastLock = wifi.createMulticastLock("blip-discovery");
            multicastLock.setReferenceCounted(false);
            multicastLock.acquire();
        }

        NotificationChannel channel = new NotificationChannel(
                CHANNEL_ID, "后台连接", NotificationManager.IMPORTANCE_LOW);
        channel.setDescription("保持 Blip 的连接、传输和局域网服务运行");
        getSystemService(NotificationManager.class).createNotificationChannel(channel);

        PendingIntent openApp = PendingIntent.getActivity(
                this,
                0,
                new Intent(this, BlipActivity.class),
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Notification notification = new Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Blip 正在后台运行")
                .setContentText("连接、传输和局域网服务保持活动")
                .setSmallIcon(R.mipmap.ic_launcher)
                .setContentIntent(openApp)
                .setCategory(Notification.CATEGORY_SERVICE)
                .setOngoing(true)
                .setShowWhen(false)
                .build();

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE);
        } else {
            startForeground(NOTIFICATION_ID, notification);
        }
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        if (multicastLock != null && multicastLock.isHeld()) {
            multicastLock.release();
        }
        if (wakeLock != null && wakeLock.isHeld()) {
            wakeLock.release();
        }
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
