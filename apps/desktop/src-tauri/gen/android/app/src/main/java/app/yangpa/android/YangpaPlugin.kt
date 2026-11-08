package app.yangpa.android

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import android.view.WindowManager
import android.view.inputmethod.InputMethodManager
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.Logger
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.util.UUID
import kotlin.concurrent.thread

@InvokeArg class KeepAliveArgs { var on = false; var text = "" }
@InvokeArg class OnArgs { var on = false }
@InvokeArg class UrlArgs { var url = "" }
@InvokeArg class ImportArgs { var uri = "" }
@InvokeArg class ExportArgs { var path = ""; var uri = "" }

/**
 * 휘발성 채널을 보는 동안 키보드에 입력을 학습하지 말라고 알립니다(IME_FLAG_NO_PERSONALIZED_LEARNING, 따르는 것은
 * 키보드 몫). wry가 만드는 RustWebView에 저장소 루트 `.cargo/config.toml`이 끼우는 코드가 이 값을 읽습니다.
 */
object ImeLearning {
  @Volatile var off = false
}

/** 고른 파일 복사본을 지우기까지 두는 시간. 데몬이 암호화해 넣은 뒤에는 쓰지 않습니다. */
private const val IMPORT_KEEP_MS = 60 * 60 * 1000L

/** src-tauri/src/android.rs가 부르는 Android 기능. */
@TauriPlugin
class YangpaPlugin(private val activity: Activity) : Plugin(activity) {
  private var keepAlive = false
  private var keepAliveText = ""
  private var inCall = false
  private var webView: WebView? = null

  override fun load(webView: WebView) {
    this.webView = webView
    // 지난 실행에서 보내지 않고 남은 복사본을 비웁니다.
    File(activity.cacheDir, "imports").deleteRecursively()
  }

  private fun applyService() {
    try {
      if (keepAlive || inCall) YangpaService.update(activity, keepAliveText, inCall) else YangpaService.stop(activity)
    } catch (e: IllegalStateException) {
      // 앱이 뒤에 있으면 서비스를 새로 시작할 수 없습니다. 도는 서비스는 그대로 두고 다음에 앞으로 나올 때 맞춥니다.
      Logger.warn("포그라운드 서비스를 바꾸지 못했습니다: ${e.message}")
    }
  }

  @Command
  fun keepAlive(invoke: Invoke) {
    val args = invoke.parseArgs(KeepAliveArgs::class.java)
    keepAlive = args.on
    keepAliveText = args.text
    applyService()
    invoke.resolve()
  }

  @Command
  fun callActive(invoke: Invoke) {
    inCall = invoke.parseArgs(OnArgs::class.java).on
    applyService()
    invoke.resolve()
  }

  @Command
  fun setSecure(invoke: Invoke) {
    val on = invoke.parseArgs(OnArgs::class.java).on
    activity.runOnUiThread {
      if (on) activity.window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
      else activity.window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
      // 휘발성 채널을 보는 동안(FLAG_SECURE와 같은 때) 키보드 학습을 끕니다. 떠 있는 키보드에도 바로 알립니다.
      if (ImeLearning.off != on) {
        ImeLearning.off = on
        webView?.let { (activity.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager).restartInput(it) }
      }
      invoke.resolve()
    }
  }

  @Command
  fun minimize(invoke: Invoke) {
    activity.moveTaskToBack(true)
    invoke.resolve()
  }

  /** Arti를 같은 프로세스에서 다시 만들 수 없어 프로세스를 새로 띄웁니다. 화면이 뒤에 있으면 다음에 열 때 뜹니다. */
  @Command
  fun restart(invoke: Invoke) {
    invoke.resolve()
    val launch = activity.packageManager.getLaunchIntentForPackage(activity.packageName)
    activity.startActivity(Intent.makeRestartActivityTask(launch?.component))
    Runtime.getRuntime().exit(0)
  }

  @Command
  fun openUrl(invoke: Invoke) {
    val url = invoke.parseArgs(UrlArgs::class.java).url
    activity.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    invoke.resolve()
  }

  @Command
  fun nativeLibDir(invoke: Invoke) {
    invoke.resolve(JSObject().put("path", activity.applicationInfo.nativeLibraryDir))
  }

  private fun displayName(uri: Uri): String? =
    activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
      if (it.moveToFirst()) it.getString(0) else null
    }

  /** 데몬은 경로만 읽으므로 고른 파일을 앱 캐시로 복사합니다. 보낼 때 암호화한 사본을 따로 두어 복사본은 곧 지웁니다. */
  @Command
  fun fileImport(invoke: Invoke) {
    val uri = Uri.parse(invoke.parseArgs(ImportArgs::class.java).uri)
    thread {
      try {
        val root = File(activity.cacheDir, "imports")
        val cutoff = System.currentTimeMillis() - IMPORT_KEEP_MS
        root.listFiles()?.filter { it.lastModified() < cutoff }?.forEach { it.deleteRecursively() }
        val dir = File(root, UUID.randomUUID().toString()).apply { mkdirs() }
        val out = File(dir, File(displayName(uri) ?: "file").name)
        activity.contentResolver.openInputStream(uri)!!.use { src -> out.outputStream().use { src.copyTo(it) } }
        invoke.resolve(JSObject().put("path", out.absolutePath))
      } catch (e: Exception) {
        invoke.reject(e.message ?: "파일을 읽지 못했습니다")
      }
    }
  }

  @Command
  fun fileExport(invoke: Invoke) {
    val args = invoke.parseArgs(ExportArgs::class.java)
    thread {
      val temp = File(args.path)
      try {
        activity.contentResolver.openOutputStream(Uri.parse(args.uri), "wt")!!.use { dst ->
          temp.inputStream().use { it.copyTo(dst) }
        }
        invoke.resolve()
      } catch (e: Exception) {
        invoke.reject(e.message ?: "파일을 쓰지 못했습니다")
      } finally {
        temp.delete()
      }
    }
  }
}
