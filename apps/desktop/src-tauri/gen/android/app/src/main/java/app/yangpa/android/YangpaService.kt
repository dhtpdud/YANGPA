package app.yangpa.android

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat

/**
 * 서버 푸시가 없어 프로세스가 살아 있어야 메시지를 받습니다. 데몬은 앱 프로세스 안에서 돌고, 이 서비스는 프로세스를
 * 살려 두기만 합니다. 프로세스가 죽으면 데몬을 띄울 Rust 쪽이 없어 다시 시작하지 않습니다(START_NOT_STICKY).
 */
class YangpaService : Service() {
  companion object {
    private const val CHANNEL = "keep_alive"
    private const val NOTIFICATION = 1
    private const val EXTRA_TEXT = "text"
    private const val EXTRA_MIC = "mic"

    fun update(context: Context, text: String, mic: Boolean) {
      val intent = Intent(context, YangpaService::class.java).putExtra(EXTRA_TEXT, text).putExtra(EXTRA_MIC, mic)
      ContextCompat.startForegroundService(context, intent)
    }

    fun stop(context: Context) {
      context.stopService(Intent(context, YangpaService::class.java))
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val text = intent?.getStringExtra(EXTRA_TEXT).orEmpty()
    val manager = getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(NotificationChannel(CHANNEL, text, NotificationManager.IMPORTANCE_MIN))
    val open = packageManager.getLaunchIntentForPackage(packageName)
    val tap = PendingIntent.getActivity(this, 0, open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    val notification = NotificationCompat.Builder(this, CHANNEL)
      .setSmallIcon(R.drawable.ic_stat_yangpa)
      .setContentTitle(getString(R.string.app_name))
      .setContentText(text)
      .setContentIntent(tap)
      .setOngoing(true)
      .setSilent(true)
      .build()
    var type = 0
    if (Build.VERSION.SDK_INT >= 34) type = ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
    // 권한 없이 마이크 종류를 주면 Android 14부터 앱이 죽습니다.
    val micGranted = ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
    if (intent?.getBooleanExtra(EXTRA_MIC, false) == true && micGranted && Build.VERSION.SDK_INT >= 30) {
      type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
    }
    ServiceCompat.startForeground(this, NOTIFICATION, notification, type)
    return START_NOT_STICKY
  }
}
