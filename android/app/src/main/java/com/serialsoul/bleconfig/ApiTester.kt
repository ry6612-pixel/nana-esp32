package com.serialsoul.bleconfig

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

object ApiTester {

    data class TestResult(val success: Boolean, val title: String, val detail: String = "")

    suspend fun testGeminiKey(key: String): TestResult = withContext(Dispatchers.IO) {
        if (key.isBlank() || !key.startsWith("AIza")) {
            return@withContext TestResult(false, "格式錯誤", "Gemini Key 應以 AIza 開頭")
        }
        try {
            val url = URL(
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key=$key"
            )
            val conn = url.openConnection() as HttpURLConnection
            conn.requestMethod = "POST"
            conn.setRequestProperty("Content-Type", "application/json")
            conn.doOutput = true
            conn.connectTimeout = 15000
            conn.readTimeout = 15000

            val body = """{"contents":[{"parts":[{"text":"Reply OK"}]}]}"""
            conn.outputStream.use { it.write(body.toByteArray()) }

            val code = conn.responseCode
            when {
                code == 200 -> TestResult(true, "API Key 有效", "Gemini API 回應正常")
                code == 400 -> TestResult(false, "請求錯誤", "API 格式有問題")
                code in 401..403 -> TestResult(false, "Key 無效或過期", "請確認 API Key 是否正確")
                code == 429 -> TestResult(false, "超過配額", "API 呼叫次數已達上限")
                else -> {
                    val err = try {
                        conn.errorStream?.bufferedReader()?.readText()?.take(200)
                    } catch (_: Exception) { null }
                    TestResult(false, "錯誤 $code", err ?: "未知錯誤")
                }
            }
        } catch (e: java.net.UnknownHostException) {
            TestResult(false, "無法連線", "請確認手機已連上網路")
        } catch (e: java.net.SocketTimeoutException) {
            TestResult(false, "連線逾時", "伺服器無回應，請稍後再試")
        } catch (e: Exception) {
            TestResult(false, "連線失敗", e.message?.take(100) ?: "未知錯誤")
        }
    }

    suspend fun testTelegramToken(token: String): TestResult = withContext(Dispatchers.IO) {
        if (token.isBlank() || !token.contains(":")) {
            return@withContext TestResult(false, "格式錯誤", "Token 格式應為 數字:字串")
        }
        try {
            val url = URL("https://api.telegram.org/bot$token/getMe")
            val conn = url.openConnection() as HttpURLConnection
            conn.requestMethod = "GET"
            conn.connectTimeout = 15000
            conn.readTimeout = 15000

            val code = conn.responseCode
            if (code == 200) {
                val response = conn.inputStream.bufferedReader().readText()
                val json = JSONObject(response)
                if (json.optBoolean("ok", false)) {
                    val result = json.getJSONObject("result")
                    val botName = result.optString("username", "unknown")
                    val firstName = result.optString("first_name", "")
                    TestResult(true, "Token 有效", "Bot: @$botName ($firstName)")
                } else {
                    TestResult(false, "Token 無效", "API 返回錯誤")
                }
            } else {
                TestResult(false, "Token 無效", "HTTP $code — 請確認 Token 是否正確")
            }
        } catch (e: java.net.UnknownHostException) {
            TestResult(false, "無法連線", "請確認手機已連上網路")
        } catch (e: java.net.SocketTimeoutException) {
            TestResult(false, "連線逾時", "Telegram 伺服器無回應")
        } catch (e: Exception) {
            TestResult(false, "連線失敗", e.message?.take(100) ?: "未知錯誤")
        }
    }
}
