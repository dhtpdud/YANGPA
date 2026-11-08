package app.yangpa.android

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.view.View
import android.webkit.WebView
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.webkit.ProxyConfig
import androidx.webkit.ProxyController
import androidx.webkit.WebViewFeature

class MainActivity : TauriActivity() {
  companion object {
    private var opened = false
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    // 최근 앱에서 밀어 닫아도 프로세스는 메시지를 받으려고 남습니다. Tauri 플러그인은 첫 화면에 묶여 있어 같은
    // 프로세스에서 화면을 새로 쓸 수 없으므로, 다시 열 때는 프로세스를 새로 띄웁니다.
    if (opened) {
      startActivity(Intent(intent).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
      Runtime.getRuntime().exit(0)
    }
    opened = true
    enableEdgeToEdge(SystemBarStyle.dark(Color.TRANSPARENT), SystemBarStyle.dark(Color.TRANSPARENT))
    super.onCreate(savedInstanceState)
    // targetSdk 35부터 앱이 화면 전체를 써서, 시스템 막대·키보드만큼 웹뷰를 안쪽으로 밉니다.
    ViewCompat.setOnApplyWindowInsetsListener(findViewById<View>(android.R.id.content)) { v, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime()
      )
      v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
    if (Build.VERSION.SDK_INT >= 33 &&
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
    ) {
      ActivityCompat.requestPermissions(this, arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
    }
  }

  // 입력 칸(복구 문구, 휘발성 채널의 글)이 자동 완성 서비스로 가지 않게 합니다. 웹뷰는 importantForAutofill을
  // 따르지 않고, 자동 완성이 없는 기기처럼 null을 받아야 끕니다.
  override fun getSystemService(name: String): Any? =
    if (name == "autofill") null else super.getSystemService(name)

  override fun onWebViewCreate(webView: WebView) {
    // 웹뷰는 입력 칸이 보이면 Tor 밖에서 구글에 양식을 묻고 이를 끄는 API가 없어서, 그 주소만 없는 프록시로 보냅니다.
    if (WebViewFeature.isFeatureSupported(WebViewFeature.PROXY_OVERRIDE) &&
      WebViewFeature.isFeatureSupported(WebViewFeature.PROXY_OVERRIDE_REVERSE_BYPASS)
    ) {
      val config = ProxyConfig.Builder()
        .addProxyRule("127.0.0.1:1")
        .addBypassRule("content-autofill.googleapis.com")
        .setReverseBypassEnabled(true)
        .build()
      ProxyController.getInstance().setProxyOverride(config, Runnable::run) {}
    }
  }
}
