package app.yangpa.android

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.core.app.NotificationCompat

/**
 * 메시지 알림. 화면이 닫힌 뒤에도 띄워야 해서 Tauri 알림 플러그인(화면이 없으면 패닉) 대신 앱 컨텍스트로 씁니다.
 * src-tauri/src/android.rs가 JNI로 부릅니다. 누르면 `link`(`yangpa://open/…`)로 앱을 엽니다.
 */
object YangpaNotifier {
  private const val CHANNEL = "messages"

  @JvmStatic
  fun show(context: Context, id: Int, channelName: String, title: String, body: String?, link: String?) {
    val manager = context.getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(NotificationChannel(CHANNEL, channelName, NotificationManager.IMPORTANCE_HIGH))
    val open = if (link != null) {
      Intent(Intent.ACTION_VIEW, Uri.parse(link), context, MainActivity::class.java)
    } else {
      context.packageManager.getLaunchIntentForPackage(context.packageName)
    }
    val tap = PendingIntent.getActivity(context, id, open, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
    val notification = NotificationCompat.Builder(context, CHANNEL)
      .setSmallIcon(R.drawable.ic_stat_yangpa)
      .setContentTitle(title)
      .setContentText(body)
      .setStyle(NotificationCompat.BigTextStyle().bigText(body))
      .setContentIntent(tap)
      .setAutoCancel(true)
      .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
      .build()
    manager.notify(id, notification)
  }
}
