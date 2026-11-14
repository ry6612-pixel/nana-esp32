package com.serialsoul.bleconfig

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.*
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import kotlinx.coroutines.launch

// ═══════════════════════════════════════════════════════════
//  Cyberpunk Color Palette
// ═══════════════════════════════════════════════════════════

private val BgDeep      = Color(0xFF060A14)
private val BgCard      = Color(0xFF101830)
private val BgInput     = Color(0xFF0C1428)
private val Cyan        = Color(0xFF00E5FF)
private val Purple      = Color(0xFF7C4DFF)
private val NeonGreen   = Color(0xFF00E676)
private val NeonRed     = Color(0xFFFF1744)
private val NeonAmber   = Color(0xFFFFD740)
private val TextMain    = Color(0xFFE0E8F5)
private val TextDim     = Color(0xFF5A7090)
private val BorderDim   = Color(0xFF1A2A50)
private val BorderGlow  = Color(0xFF00E5FF).copy(alpha = 0.25f)

// ═══════════════════════════════════════════════════════════
//  Main Activity
// ═══════════════════════════════════════════════════════════

class MainActivity : ComponentActivity() {

    private lateinit var bleManager: BleManager

    private val permLauncher = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) { grants ->
        if (grants.values.all { it }) bleManager.startScan()
        else Toast.makeText(this, "需要藍牙和定位權限才能掃描", Toast.LENGTH_LONG).show()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        bleManager = BleManager(this)

        setContent {
            MaterialTheme(
                colorScheme = darkColorScheme(
                    primary      = Cyan,
                    secondary    = Purple,
                    background   = BgDeep,
                    surface      = BgCard,
                    surfaceVariant = BgCard,
                    onPrimary    = Color.Black,
                    onBackground = TextMain,
                    onSurface    = TextMain,
                    outline      = BorderDim,
                )
            ) {
                SerialSoulApp(bleManager, ::requestPermAndScan)
            }
        }
    }

    private fun requestPermAndScan() {
        val perms = mutableListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            perms.add(Manifest.permission.BLUETOOTH_SCAN)
            perms.add(Manifest.permission.BLUETOOTH_CONNECT)
        }
        perms.add(Manifest.permission.ACCESS_FINE_LOCATION)
        val needed = perms.filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (needed.isEmpty()) bleManager.startScan()
        else permLauncher.launch(needed.toTypedArray())
    }

    override fun onDestroy() {
        super.onDestroy()
        bleManager.disconnect()
    }
}

// ═══════════════════════════════════════════════════════════
//  Root
// ═══════════════════════════════════════════════════════════

@Composable
fun SerialSoulApp(ble: BleManager, onScan: () -> Unit) {
    val connState by ble.state.collectAsState()
    var demoMode by remember { mutableStateOf(false) }

    Surface(modifier = Modifier.fillMaxSize(), color = BgDeep) {
        if (demoMode) {
            DemoConfigScreen(onExit = { demoMode = false })
        } else {
            when (connState) {
                is BleManager.ConnectionState.Disconnected,
                is BleManager.ConnectionState.Scanning -> ScanScreen(ble, onScan, onDemo = { demoMode = true })
                is BleManager.ConnectionState.Connecting -> ConnectingScreen()
                is BleManager.ConnectionState.Connected -> AuthScreen(ble)
                is BleManager.ConnectionState.Authenticated -> ConfigScreen(ble)
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
//  Reusable Components
// ═══════════════════════════════════════════════════════════

@Composable
fun GlowCard(
    modifier: Modifier = Modifier,
    borderColor: Color = BorderGlow,
    content: @Composable ColumnScope.() -> Unit
) {
    val shape = RoundedCornerShape(16.dp)
    Card(
        modifier = modifier
            .fillMaxWidth()
            .border(1.dp, borderColor, shape),
        shape = shape,
        colors = CardDefaults.cardColors(containerColor = BgCard),
        elevation = CardDefaults.cardElevation(0.dp)
    ) {
        Column(modifier = Modifier.padding(16.dp), content = content)
    }
}

@Composable
fun SectionLabel(text: String) {
    Text(
        text.uppercase(),
        fontSize = 11.sp,
        fontWeight = FontWeight.Bold,
        fontFamily = FontFamily.Monospace,
        color = Cyan,
        letterSpacing = 2.sp,
        modifier = Modifier.padding(bottom = 8.dp)
    )
}

@Composable
fun CyberTextField(
    value: String,
    onValueChange: (String) -> Unit,
    label: String,
    modifier: Modifier = Modifier,
    isSecret: Boolean = false,
    isModified: Boolean = false,
    leadingIcon: ImageVector? = null,
    hint: String = ""
) {
    var showSecret by remember { mutableStateOf(false) }

    OutlinedTextField(
        value = value,
        onValueChange = onValueChange,
        label = { Text(label, fontSize = 12.sp) },
        placeholder = if (hint.isNotEmpty()) {
            { Text(hint, color = TextDim.copy(alpha = 0.4f), fontSize = 13.sp) }
        } else null,
        modifier = modifier.fillMaxWidth(),
        singleLine = true,
        textStyle = TextStyle(fontSize = 14.sp, fontFamily = FontFamily.Monospace, color = TextMain),
        leadingIcon = if (leadingIcon != null) {
            { Icon(leadingIcon, null, tint = Cyan, modifier = Modifier.size(20.dp)) }
        } else null,
        visualTransformation = if (isSecret && !showSecret) PasswordVisualTransformation() else VisualTransformation.None,
        trailingIcon = {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (isModified) {
                    Box(
                        modifier = Modifier
                            .size(8.dp)
                            .clip(CircleShape)
                            .background(NeonAmber)
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                }
                if (isSecret) {
                    IconButton(onClick = { showSecret = !showSecret }, modifier = Modifier.size(32.dp)) {
                        Icon(
                            if (showSecret) Icons.Default.VisibilityOff else Icons.Default.Visibility,
                            null,
                            tint = TextDim,
                            modifier = Modifier.size(18.dp)
                        )
                    }
                }
            }
        },
        colors = OutlinedTextFieldDefaults.colors(
            focusedBorderColor = if (isModified) NeonAmber else Cyan,
            unfocusedBorderColor = if (isModified) NeonAmber.copy(alpha = 0.4f) else BorderDim,
            focusedContainerColor = BgInput,
            unfocusedContainerColor = BgInput,
            cursorColor = Cyan,
            focusedLabelColor = Cyan,
            unfocusedLabelColor = TextDim,
        ),
        shape = RoundedCornerShape(12.dp)
    )
}

private fun maskSecret(value: String, showFirst: Int = 4, showLast: Int = 4): String {
    if (value.length <= showFirst + showLast + 4) return "\u25CF".repeat(value.length.coerceAtLeast(1))
    return value.take(showFirst) + "\u25CF\u25CF\u25CF\u25CF" + value.takeLast(showLast)
}

private fun signalColor(rssi: Int): Color = when {
    rssi >= -50 -> NeonGreen
    rssi >= -70 -> NeonAmber
    else -> NeonRed
}

// ═══════════════════════════════════════════════════════════
//  API Test State
// ═══════════════════════════════════════════════════════════

sealed class ApiTestState {
    data object Idle : ApiTestState()
    data object Testing : ApiTestState()
    data class Success(val title: String, val detail: String) : ApiTestState()
    data class Failed(val title: String, val detail: String) : ApiTestState()
}

// ═══════════════════════════════════════════════════════════
//  Scan Screen
// ═══════════════════════════════════════════════════════════

@Composable
fun ScanScreen(ble: BleManager, onScan: () -> Unit, onDemo: () -> Unit = {}) {
    val connState by ble.state.collectAsState()
    val devices by ble.devices.collectAsState()
    val isScanning = connState is BleManager.ConnectionState.Scanning

    val infiniteTransition = rememberInfiniteTransition(label = "scan")
    val pulseAlpha by infiniteTransition.animateFloat(
        initialValue = 0.4f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(1200, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Reverse
        ),
        label = "pulse"
    )

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(BgDeep)
            .padding(24.dp)
    ) {
        Spacer(modifier = Modifier.height(48.dp))

        // Logo
        Text(
            "SERIAL",
            fontSize = 40.sp,
            fontWeight = FontWeight.ExtraBold,
            fontFamily = FontFamily.Monospace,
            color = Cyan,
            letterSpacing = 10.sp
        )
        Text(
            "SOUL",
            fontSize = 40.sp,
            fontWeight = FontWeight.ExtraBold,
            fontFamily = FontFamily.Monospace,
            color = Purple,
            letterSpacing = 10.sp
        )
        Spacer(modifier = Modifier.height(4.dp))
        Text(
            "BLE CONFIGURATION SYSTEM",
            fontSize = 10.sp,
            fontFamily = FontFamily.Monospace,
            color = TextDim,
            letterSpacing = 3.sp
        )

        Spacer(modifier = Modifier.height(40.dp))

        // Scan Button
        Button(
            onClick = { if (isScanning) ble.stopScan() else onScan() },
            modifier = Modifier
                .fillMaxWidth()
                .height(56.dp)
                .then(if (isScanning) Modifier.alpha(pulseAlpha) else Modifier),
            shape = RoundedCornerShape(16.dp),
            colors = ButtonDefaults.buttonColors(
                containerColor = if (isScanning) NeonRed.copy(alpha = 0.12f) else Cyan.copy(alpha = 0.10f)
            ),
            border = BorderStroke(1.dp, if (isScanning) NeonRed else Cyan)
        ) {
            Icon(
                Icons.Default.BluetoothSearching,
                null,
                tint = if (isScanning) NeonRed else Cyan,
                modifier = Modifier.size(22.dp)
            )
            Spacer(modifier = Modifier.width(12.dp))
            Text(
                if (isScanning) "搜尋中\u2026 點擊停止" else "掃描 BLE 裝置",
                color = if (isScanning) NeonRed else Cyan,
                fontWeight = FontWeight.Bold,
                fontFamily = FontFamily.Monospace,
                fontSize = 15.sp
            )
        }

        Spacer(modifier = Modifier.height(24.dp))

        if (isScanning && devices.isEmpty()) {
            Column(
                modifier = Modifier.fillMaxWidth(),
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                CircularProgressIndicator(
                    modifier = Modifier.size(32.dp),
                    color = Cyan,
                    strokeWidth = 2.dp
                )
                Spacer(modifier = Modifier.height(12.dp))
                Text(
                    "搜尋 SerialSoul 裝置中\u2026",
                    color = TextDim,
                    fontSize = 13.sp,
                    fontFamily = FontFamily.Monospace
                )
            }
        }

        // Device List
        LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            items(devices) { dev ->
                GlowCard(
                    modifier = Modifier.clickable { ble.connect(dev.device) },
                    borderColor = Cyan.copy(alpha = 0.2f)
                ) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Box(
                            modifier = Modifier
                                .size(44.dp)
                                .clip(RoundedCornerShape(12.dp))
                                .background(Cyan.copy(alpha = 0.1f)),
                            contentAlignment = Alignment.Center
                        ) {
                            Icon(Icons.Default.Bluetooth, null, tint = Cyan, modifier = Modifier.size(24.dp))
                        }
                        Spacer(modifier = Modifier.width(16.dp))
                        Column(modifier = Modifier.weight(1f)) {
                            Text(dev.name, fontWeight = FontWeight.Bold, fontSize = 15.sp, color = TextMain)
                            Text(dev.address, fontSize = 11.sp, fontFamily = FontFamily.Monospace, color = TextDim)
                        }
                        Surface(
                            shape = RoundedCornerShape(8.dp),
                            color = signalColor(dev.rssi).copy(alpha = 0.15f)
                        ) {
                            Text(
                                "${dev.rssi} dBm",
                                modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
                                fontSize = 11.sp,
                                fontFamily = FontFamily.Monospace,
                                color = signalColor(dev.rssi)
                            )
                        }
                    }
                }
            }
        }

        Spacer(modifier = Modifier.weight(1f))

        TextButton(onClick = onDemo, modifier = Modifier.align(Alignment.CenterHorizontally)) {
            Text("DEMO MODE", fontSize = 10.sp, fontFamily = FontFamily.Monospace, color = TextDim.copy(alpha = 0.5f))
        }

        Text(
            "v1.0.0 \u2014 SerialSoul BLE Config",
            fontSize = 10.sp,
            fontFamily = FontFamily.Monospace,
            color = TextDim.copy(alpha = 0.4f),
            modifier = Modifier.align(Alignment.CenterHorizontally)
        )
    }
}

// ═══════════════════════════════════════════════════════════
//  Connecting Screen
// ═══════════════════════════════════════════════════════════

@Composable
fun ConnectingScreen() {
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(BgDeep),
        contentAlignment = Alignment.Center
    ) {
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            CircularProgressIndicator(modifier = Modifier.size(56.dp), color = Cyan, strokeWidth = 3.dp)
            Spacer(modifier = Modifier.height(24.dp))
            Text(
                "CONNECTING",
                fontSize = 18.sp,
                fontWeight = FontWeight.Bold,
                fontFamily = FontFamily.Monospace,
                color = Cyan,
                letterSpacing = 4.sp
            )
            Spacer(modifier = Modifier.height(8.dp))
            Text("正在建立 BLE 連線\u2026", fontSize = 13.sp, color = TextDim)
        }
    }
}

// ═══════════════════════════════════════════════════════════
//  Auth Screen
// ═══════════════════════════════════════════════════════════

@Composable
fun AuthScreen(ble: BleManager) {
    var pin by remember { mutableStateOf("") }
    var showPin by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf("") }
    var loading by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(BgDeep)
            .padding(32.dp),
        contentAlignment = Alignment.Center
    ) {
        GlowCard(borderColor = if (error.isNotEmpty()) NeonRed.copy(alpha = 0.5f) else Cyan.copy(alpha = 0.3f)) {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Box(
                    modifier = Modifier
                        .size(64.dp)
                        .clip(CircleShape)
                        .background(Cyan.copy(alpha = 0.1f)),
                    contentAlignment = Alignment.Center
                ) {
                    Icon(Icons.Default.Lock, null, tint = Cyan, modifier = Modifier.size(32.dp))
                }
                Spacer(modifier = Modifier.height(16.dp))
                Text(
                    "AUTHENTICATION",
                    fontSize = 16.sp,
                    fontWeight = FontWeight.Bold,
                    fontFamily = FontFamily.Monospace,
                    color = Cyan,
                    letterSpacing = 3.sp
                )
                Spacer(modifier = Modifier.height(4.dp))
                Text("輸入配對 PIN（首次配對時顯示在裝置螢幕上）", fontSize = 13.sp, color = TextDim)
                Spacer(modifier = Modifier.height(24.dp))

                OutlinedTextField(
                    value = pin,
                    onValueChange = { pin = it; error = "" },
                    label = { Text("密碼") },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                    textStyle = TextStyle(
                        fontFamily = FontFamily.Monospace,
                        fontSize = 18.sp,
                        letterSpacing = 4.sp,
                        textAlign = TextAlign.Center
                    ),
                    visualTransformation = if (showPin) VisualTransformation.None else PasswordVisualTransformation('\u25CF'),
                    trailingIcon = {
                        IconButton(onClick = { showPin = !showPin }) {
                            Icon(
                                if (showPin) Icons.Default.VisibilityOff else Icons.Default.Visibility,
                                null,
                                tint = TextDim
                            )
                        }
                    },
                    colors = OutlinedTextFieldDefaults.colors(
                        focusedBorderColor = Cyan,
                        unfocusedBorderColor = BorderDim,
                        focusedContainerColor = BgInput,
                        unfocusedContainerColor = BgInput,
                        cursorColor = Cyan,
                    ),
                    shape = RoundedCornerShape(12.dp)
                )

                if (error.isNotEmpty()) {
                    Spacer(modifier = Modifier.height(8.dp))
                    Text(error, color = NeonRed, fontSize = 13.sp)
                }

                Spacer(modifier = Modifier.height(20.dp))

                Button(
                    onClick = {
                        if (pin.isEmpty()) { error = "請輸入 PIN"; return@Button }
                        loading = true
                        scope.launch {
                            val resp = ble.authenticateDetailed(pin)
                            loading = false
                            error = when {
                                resp == "OK" -> ""
                                resp.startsWith("LOCKED") ->
                                    "錯誤次數過多，裝置已鎖定 ${resp.removePrefix("LOCKED").trim()} 秒"
                                resp.startsWith("ERR") -> "裝置無回應，請重試"
                                else -> "PIN 錯誤，請重試"
                            }
                        }
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(48.dp),
                    shape = RoundedCornerShape(12.dp),
                    colors = ButtonDefaults.buttonColors(containerColor = Cyan.copy(alpha = 0.15f)),
                    border = BorderStroke(1.dp, Cyan),
                    enabled = !loading
                ) {
                    if (loading) {
                        CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp, color = Cyan)
                    } else {
                        Text("解鎖", color = Cyan, fontWeight = FontWeight.Bold, fontFamily = FontFamily.Monospace)
                    }
                }

                Spacer(modifier = Modifier.height(8.dp))
                TextButton(onClick = { ble.disconnect() }) {
                    Text("斷線返回", color = TextDim, fontSize = 12.sp)
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
//  Config Screen (Tab-based)
// ═══════════════════════════════════════════════════════════

private enum class ConfigTab(val label: String, val icon: ImageVector) {
    CHAT("對話", Icons.Default.Chat),
    WIFI("WiFi", Icons.Default.Wifi),
    BOT("Bot", Icons.Default.Send),
    AI("AI", Icons.Default.AutoAwesome),
    CAMERA("\u76F8\u6A5F", Icons.Default.PhotoCamera),
    SYSTEM("\u7CFB\u7D71", Icons.Default.Settings),
    LOG("Log", Icons.Default.Code)
}

@Composable
fun ConfigScreen(ble: BleManager) {
    val scope = rememberCoroutineScope()
    val logLines by ble.log.collectAsState()
    var selectedTab by remember { mutableStateOf(ConfigTab.CHAT) }
    var fwVersion by remember { mutableStateOf("") }
    var saving by remember { mutableStateOf(false) }
    var statusMsg by remember { mutableStateOf("") }
    var statusOk by remember { mutableStateOf(true) }
    var showWizard by remember { mutableStateOf(false) }

    val values = remember { mutableStateMapOf<String, String>() }
    val modified = remember { mutableStateMapOf<String, Boolean>() }

    LaunchedEffect(Unit) {
        fwVersion = ble.ping()
        val keys = listOf(
            "wifi_ssid", "wifi_pass",
            "wifi_ssid2", "wifi_pass2",
            "wifi_ssid3", "wifi_pass3",
            "tg_token", "chat_id",
            "gemini_key", "model_pref",
            "voice_mode", "wake_phrase", "ble_pin",
            "cam_enable", "cam_task", "cam_quality"
        )
        for (k in keys) {
            val v = ble.getConfig(k)
            values[k] = if (v.startsWith("ERR")) "" else v
        }
        // Auto-launch wizard if WiFi not configured yet
        if (values["wifi_ssid"].isNullOrBlank()) showWizard = true
    }

    fun onFieldChange(key: String, value: String) {
        values[key] = value
        modified[key] = true
    }

    Box(modifier = Modifier.fillMaxSize()) {
    Column(modifier = Modifier
        .fillMaxSize()
        .background(BgDeep)) {

        // ── Top Bar ──
        Surface(color = BgCard, tonalElevation = 0.dp) {
            Column {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 12.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    Box(
                        modifier = Modifier
                            .size(36.dp)
                            .clip(RoundedCornerShape(10.dp))
                            .background(Cyan.copy(alpha = 0.1f)),
                        contentAlignment = Alignment.Center
                    ) {
                        Icon(Icons.Default.Bluetooth, null, tint = Cyan, modifier = Modifier.size(20.dp))
                    }
                    Spacer(modifier = Modifier.width(12.dp))
                    Column(modifier = Modifier.weight(1f)) {
                        Text(
                            "SERIAL SOUL",
                            fontWeight = FontWeight.Bold,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 16.sp,
                            color = Cyan,
                            letterSpacing = 2.sp
                        )
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Box(
                                modifier = Modifier
                                    .size(6.dp)
                                    .clip(CircleShape)
                                    .background(NeonGreen)
                            )
                            Spacer(modifier = Modifier.width(6.dp))
                            Text(
                                fwVersion.ifEmpty { "\u5DF2\u9023\u7DDA" },
                                fontSize = 11.sp,
                                fontFamily = FontFamily.Monospace,
                                color = TextDim
                            )
                        }
                    }
                    IconButton(onClick = { ble.disconnect() }) {
                        Icon(Icons.Default.PowerSettingsNew, null, tint = NeonRed, modifier = Modifier.size(22.dp))
                    }
                }

                // Tab Bar
                ScrollableTabRow(
                    selectedTabIndex = ConfigTab.values().indexOf(selectedTab),
                    containerColor = Color.Transparent,
                    contentColor = Cyan,
                    edgePadding = 8.dp,
                    indicator = { tabPositions ->
                        val idx = ConfigTab.values().indexOf(selectedTab)
                        if (idx in tabPositions.indices) {
                            with(TabRowDefaults) {
                                Indicator(
                                    modifier = Modifier.tabIndicatorOffset(tabPositions[idx]),
                                    height = 2.dp,
                                    color = Cyan
                                )
                            }
                        }
                    },
                    divider = { Divider(color = BorderDim, thickness = 1.dp) }
                ) {
                    ConfigTab.values().forEach { tab ->
                        Tab(
                            selected = selectedTab == tab,
                            onClick = { selectedTab = tab },
                            text = {
                                Text(
                                    tab.label,
                                    fontSize = 12.sp,
                                    fontWeight = if (selectedTab == tab) FontWeight.Bold else FontWeight.Normal,
                                    fontFamily = FontFamily.Monospace
                                )
                            },
                            icon = {
                                Icon(
                                    tab.icon,
                                    null,
                                    modifier = Modifier.size(18.dp),
                                    tint = if (selectedTab == tab) Cyan else TextDim
                                )
                            },
                            selectedContentColor = Cyan,
                            unselectedContentColor = TextDim
                        )
                    }
                }
            }
        }

        // ── Status Bar ──
        AnimatedVisibility(visible = statusMsg.isNotEmpty()) {
            Surface(
                modifier = Modifier.fillMaxWidth(),
                color = if (statusOk) NeonGreen.copy(alpha = 0.1f) else NeonRed.copy(alpha = 0.1f)
            ) {
                Row(
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    Icon(
                        if (statusOk) Icons.Default.CheckCircle else Icons.Default.Error,
                        null,
                        tint = if (statusOk) NeonGreen else NeonRed,
                        modifier = Modifier.size(16.dp)
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                    Text(statusMsg, fontSize = 12.sp, color = if (statusOk) NeonGreen else NeonRed)
                }
            }
        }

        // ── Tab Content ──
        Box(modifier = Modifier.weight(1f)) {
            when (selectedTab) {
                ConfigTab.CHAT   -> ChatTab(
                    sendMessage = { ble.sendChat(it) },
                    getStatus = { ble.getStatus() }
                )
                ConfigTab.WIFI   -> WifiTab(values, modified) { k, v -> onFieldChange(k, v) }
                ConfigTab.BOT    -> BotTab(values, modified) { k, v -> onFieldChange(k, v) }
                ConfigTab.AI     -> AiTab(values, modified) { k, v -> onFieldChange(k, v) }
                ConfigTab.CAMERA -> CameraTab(values, modified) { k, v -> onFieldChange(k, v) }
                ConfigTab.SYSTEM -> SystemTab(values, modified, fwVersion, ble.connectedName) { k, v -> onFieldChange(k, v) }
                ConfigTab.LOG    -> LogTab(logLines)
            }
        }

        // ── Bottom Action Bar (hidden for Chat tab) ──
        if (selectedTab != ConfigTab.CHAT) {
        Surface(color = BgCard) {
            Column {
                Divider(color = BorderDim, thickness = 1.dp)
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(12.dp),
                    horizontalArrangement = Arrangement.spacedBy(12.dp)
                ) {
                    OutlinedButton(
                        onClick = {
                            scope.launch {
                                statusMsg = ""; statusOk = true
                                val keys = values.keys.toList()
                                for (k in keys) {
                                    val v = ble.getConfig(k)
                                    values[k] = if (v.startsWith("ERR")) "" else v
                                }
                                modified.clear()
                                statusMsg = "\u5DF2\u91CD\u65B0\u8B80\u53D6\u88DD\u7F6E\u8A2D\u5B9A"; statusOk = true
                            }
                        },
                        modifier = Modifier
                            .weight(1f)
                            .height(44.dp),
                        shape = RoundedCornerShape(12.dp),
                        border = BorderStroke(1.dp, BorderDim)
                    ) {
                        Icon(Icons.Default.Refresh, null, modifier = Modifier.size(16.dp), tint = TextDim)
                        Spacer(modifier = Modifier.width(6.dp))
                        Text("\u91CD\u8B80", color = TextDim, fontSize = 13.sp)
                    }

                    Button(
                        onClick = {
                            saving = true
                            scope.launch {
                                statusMsg = ""
                                var ok = true
                                var count = 0
                                for ((key, isModified) in modified) {
                                    if (isModified) {
                                        val v = values[key] ?: continue
                                        if (!ble.setConfig(key, v)) {
                                            statusMsg = "\u5BEB\u5165 $key \u5931\u6557"; statusOk = false
                                            ok = false; break
                                        }
                                        count++
                                    }
                                }
                                if (ok && count > 0) {
                                    statusMsg = "\u5DF2\u5BEB\u5165 $count \u9805\uFF0C\u88DD\u7F6E\u91CD\u555F\u4E2D\u2026"; statusOk = true
                                    ble.saveAndRestart()
                                } else if (ok) {
                                    statusMsg = "\u6C92\u6709\u4FEE\u6539\u7684\u9805\u76EE"; statusOk = true
                                }
                                saving = false
                                modified.clear()
                            }
                        },
                        modifier = Modifier
                            .weight(1f)
                            .height(44.dp),
                        shape = RoundedCornerShape(12.dp),
                        enabled = !saving && modified.any { it.value },
                        colors = ButtonDefaults.buttonColors(
                            containerColor = NeonGreen.copy(alpha = 0.15f),
                            disabledContainerColor = BgInput
                        ),
                        border = BorderStroke(
                            1.dp,
                            if (!saving && modified.any { it.value }) NeonGreen else BorderDim
                        )
                    ) {
                        if (saving) {
                            CircularProgressIndicator(modifier = Modifier.size(16.dp), strokeWidth = 2.dp, color = NeonGreen)
                        } else {
                            Icon(
                                Icons.Default.Save, null,
                                modifier = Modifier.size(16.dp),
                                tint = if (modified.any { it.value }) NeonGreen else TextDim
                            )
                        }
                        Spacer(modifier = Modifier.width(6.dp))
                        Text(
                            "\u5132\u5B58\u4E26\u91CD\u555F",
                            color = if (!saving && modified.any { it.value }) NeonGreen else TextDim,
                            fontSize = 13.sp
                        )
                    }
                }
            }
        }
        } // end if (selectedTab != ConfigTab.CHAT)
    } // end Column

    // ── Setup Wizard Overlay ──
    if (showWizard) {
        SetupWizard(
            initialValues = values,
            onComplete = { wizardValues ->
                scope.launch {
                    for ((k, v) in wizardValues) {
                        if (v.isNotBlank()) {
                            ble.setConfig(k, v)
                            values[k] = v
                        }
                    }
                    modified.clear()
                    showWizard = false
                    ble.saveAndRestart()
                }
            },
            onSkip = { showWizard = false }
        )
    }
    } // end Box
}

// ═══════════════════════════════════════════════════════════
//  Chat Tab - AI Communication Interface
// ═══════════════════════════════════════════════════════════

private data class ChatMessage(
    val text: String,
    val isUser: Boolean,
    val timestamp: Long = System.currentTimeMillis()
)

@Composable
fun ChatTab(
    sendMessage: suspend (String) -> String,
    getStatus: suspend () -> String,
) {
    val scope = rememberCoroutineScope()
    val messages = remember { mutableStateListOf<ChatMessage>() }
    var inputText by remember { mutableStateOf("") }
    var isSending by remember { mutableStateOf(false) }
    val listState = rememberLazyListState()

    LaunchedEffect(messages.size) {
        if (messages.isNotEmpty()) listState.animateScrollToItem(messages.lastIndex)
    }

    LaunchedEffect(Unit) {
        if (messages.isEmpty()) {
            messages.add(ChatMessage(
                "歡迎使用 SerialSoul AI 對話。\n輸入訊息或使用快捷指令開始。",
                isUser = false
            ))
        }
    }

    fun doSend(text: String) {
        if (text.isBlank() || isSending) return
        val userMsg = text.trim()
        messages.add(ChatMessage(userMsg, isUser = true))
        inputText = ""
        isSending = true
        scope.launch {
            try {
                val response = if (userMsg.equals("/status", ignoreCase = true)) {
                    getStatus()
                } else {
                    sendMessage(userMsg)
                }
                val displayText = when {
                    response.startsWith("OK ") -> response.removePrefix("OK ").trim()
                    response == "ERR timeout" -> "⚠️ 回應逾時，請稍後重試"
                    response.startsWith("ERR ") -> "⚠️ ${response.removePrefix("ERR ").trim()}"
                    else -> response
                }
                messages.add(ChatMessage(displayText, isUser = false))
            } catch (e: Exception) {
                messages.add(ChatMessage("⚠️ 發送失敗: ${e.message}", isUser = false))
            } finally {
                isSending = false
            }
        }
    }

    Column(modifier = Modifier.fillMaxSize()) {
        // ── Quick Command Bar ──
        Surface(modifier = Modifier.fillMaxWidth(), color = BgCard) {
            Row(
                modifier = Modifier
                    .horizontalScroll(rememberScrollState())
                    .padding(horizontal = 12.dp, vertical = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp)
            ) {
                QuickCmdChip("📊 狀態") { doSend("/status") }
                QuickCmdChip("❓ 幫助") { doSend("/help") }
                QuickCmdChip("📷 拍照") { doSend("/snap") }
                QuickCmdChip("🌐 WiFi") { doSend("/wifi") }
            }
        }
        Divider(color = BorderDim, thickness = 1.dp)

        // ── Message List ──
        LazyColumn(
            state = listState,
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .background(BgDeep)
                .padding(horizontal = 12.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp)
        ) {
            items(messages.size) { index ->
                ChatBubble(messages[index])
            }
            if (isSending) {
                item { TypingIndicator() }
            }
        }

        // ── Input Area ──
        Surface(color = BgCard) {
            Column {
                Divider(color = BorderDim, thickness = 1.dp)
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 8.dp, vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    OutlinedTextField(
                        value = inputText,
                        onValueChange = { inputText = it },
                        modifier = Modifier.weight(1f),
                        placeholder = { Text("輸入訊息…", color = TextDim, fontSize = 14.sp) },
                        textStyle = TextStyle(color = TextMain, fontSize = 14.sp),
                        colors = OutlinedTextFieldDefaults.colors(
                            focusedBorderColor = Cyan.copy(alpha = 0.5f),
                            unfocusedBorderColor = BorderDim,
                            cursorColor = Cyan,
                            focusedContainerColor = BgInput,
                            unfocusedContainerColor = BgInput
                        ),
                        shape = RoundedCornerShape(20.dp),
                        maxLines = 3,
                        enabled = !isSending
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                    IconButton(
                        onClick = { doSend(inputText) },
                        enabled = inputText.isNotBlank() && !isSending,
                        modifier = Modifier
                            .size(44.dp)
                            .clip(CircleShape)
                            .background(
                                if (inputText.isNotBlank() && !isSending) Cyan.copy(alpha = 0.15f)
                                else BorderDim.copy(alpha = 0.3f)
                            )
                    ) {
                        Icon(
                            Icons.Default.Send, null,
                            tint = if (inputText.isNotBlank() && !isSending) Cyan else TextDim,
                            modifier = Modifier.size(22.dp)
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun ChatBubble(message: ChatMessage) {
    val alignment = if (message.isUser) Alignment.End else Alignment.Start
    val bgColor = if (message.isUser) Cyan.copy(alpha = 0.1f) else BgCard
    val borderColor = if (message.isUser) Cyan.copy(alpha = 0.3f) else NeonGreen.copy(alpha = 0.2f)
    val textColor = if (message.isUser) Cyan else NeonGreen

    Column(
        modifier = Modifier.fillMaxWidth(),
        horizontalAlignment = alignment
    ) {
        Surface(
            shape = RoundedCornerShape(
                topStart = 16.dp, topEnd = 16.dp,
                bottomStart = if (message.isUser) 16.dp else 4.dp,
                bottomEnd = if (message.isUser) 4.dp else 16.dp
            ),
            color = bgColor,
            border = BorderStroke(1.dp, borderColor),
            modifier = Modifier.widthIn(max = 300.dp)
        ) {
            Column(modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (message.isUser) "YOU" else "AI",
                        fontSize = 9.sp, fontWeight = FontWeight.Bold,
                        fontFamily = FontFamily.Monospace,
                        color = textColor.copy(alpha = 0.6f),
                        letterSpacing = 1.sp
                    )
                    Spacer(modifier = Modifier.width(8.dp))
                    Text(
                        formatChatTime(message.timestamp),
                        fontSize = 9.sp, fontFamily = FontFamily.Monospace,
                        color = TextDim.copy(alpha = 0.5f)
                    )
                }
                Spacer(modifier = Modifier.height(4.dp))
                Text(message.text, fontSize = 13.sp, color = TextMain, lineHeight = 18.sp)
            }
        }
    }
}

@Composable
private fun TypingIndicator() {
    val infiniteTransition = rememberInfiniteTransition(label = "typing")
    val alpha by infiniteTransition.animateFloat(
        initialValue = 0.3f, targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(600), repeatMode = RepeatMode.Reverse
        ), label = "alpha"
    )
    Row(modifier = Modifier.padding(start = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        Surface(
            shape = RoundedCornerShape(12.dp), color = BgCard,
            border = BorderStroke(1.dp, NeonGreen.copy(alpha = 0.2f))
        ) {
            Row(modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp)) {
                Text("AI 思考中", fontSize = 11.sp, color = NeonGreen.copy(alpha = alpha), fontFamily = FontFamily.Monospace)
                Text(" ●●●", fontSize = 11.sp, color = NeonGreen.copy(alpha = alpha), fontFamily = FontFamily.Monospace)
            }
        }
    }
}

@Composable
private fun QuickCmdChip(label: String, onClick: () -> Unit) {
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(16.dp),
        color = BgInput,
        border = BorderStroke(1.dp, BorderDim)
    ) {
        Text(
            label,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
            fontSize = 12.sp, color = TextMain, fontFamily = FontFamily.Monospace
        )
    }
}

private fun formatChatTime(timestamp: Long): String {
    val sdf = java.text.SimpleDateFormat("HH:mm", java.util.Locale.getDefault())
    return sdf.format(java.util.Date(timestamp))
}

// ═══════════════════════════════════════════════════════════
//  WiFi Tab
// ═══════════════════════════════════════════════════════════

@Composable
fun WifiTab(
    values: Map<String, String>,
    modified: Map<String, Boolean>,
    onChange: (String, String) -> Unit
) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        SectionLabel("NETWORK CONFIGURATION")

        WifiSlotCard("\u4E3B\u8981\u7DB2\u8DEF", "wifi_ssid", "wifi_pass", 1, values, modified, onChange)
        WifiSlotCard("\u5099\u7528\u7DB2\u8DEF 1", "wifi_ssid2", "wifi_pass2", 2, values, modified, onChange)
        WifiSlotCard("\u5099\u7528\u7DB2\u8DEF 2", "wifi_ssid3", "wifi_pass3", 3, values, modified, onChange)

        Surface(
            modifier = Modifier.fillMaxWidth(),
            shape = RoundedCornerShape(12.dp),
            color = Cyan.copy(alpha = 0.05f),
            border = BorderStroke(1.dp, Cyan.copy(alpha = 0.1f))
        ) {
            Row(modifier = Modifier.padding(12.dp)) {
                Icon(Icons.Default.Info, null, tint = Cyan, modifier = Modifier.size(16.dp))
                Spacer(modifier = Modifier.width(8.dp))
                Text(
                    "\u6700\u591A\u652F\u63F4 3 \u7D44 WiFi\uFF0C\u88DD\u7F6E\u6703\u81EA\u52D5\u4F9D\u5E8F\u5617\u8A66\u9023\u7DDA\u3002\u9023\u7DDA\u5931\u6557\u6642\u6703\u7121\u9650\u5FAA\u74B0\u91CD\u8A66\u3002",
                    fontSize = 11.sp,
                    color = TextDim,
                    lineHeight = 16.sp
                )
            }
        }

        Spacer(modifier = Modifier.height(60.dp))
    }
}

@Composable
fun WifiSlotCard(
    title: String,
    ssidKey: String,
    passKey: String,
    slot: Int,
    values: Map<String, String>,
    modified: Map<String, Boolean>,
    onChange: (String, String) -> Unit
) {
    val isConfigured = (values[ssidKey] ?: "").isNotBlank()

    GlowCard(borderColor = if (isConfigured) Cyan.copy(alpha = 0.2f) else BorderDim) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(
                modifier = Modifier
                    .size(32.dp)
                    .clip(RoundedCornerShape(8.dp))
                    .background(if (isConfigured) Cyan.copy(alpha = 0.1f) else BgInput),
                contentAlignment = Alignment.Center
            ) {
                Text(
                    "$slot",
                    fontSize = 14.sp,
                    fontWeight = FontWeight.Bold,
                    fontFamily = FontFamily.Monospace,
                    color = if (isConfigured) Cyan else TextDim
                )
            }
            Spacer(modifier = Modifier.width(12.dp))
            Text(title, fontWeight = FontWeight.SemiBold, fontSize = 14.sp, color = TextMain)
            Spacer(modifier = Modifier.weight(1f))
            if (isConfigured) {
                Surface(shape = RoundedCornerShape(6.dp), color = NeonGreen.copy(alpha = 0.1f)) {
                    Text(
                        "\u5DF2\u8A2D\u5B9A",
                        modifier = Modifier.padding(horizontal = 8.dp, vertical = 2.dp),
                        fontSize = 10.sp,
                        color = NeonGreen
                    )
                }
            }
        }
        Spacer(modifier = Modifier.height(12.dp))
        CyberTextField(
            value = values[ssidKey] ?: "",
            onValueChange = { onChange(ssidKey, it) },
            label = "SSID",
            leadingIcon = Icons.Default.Wifi,
            isModified = modified[ssidKey] == true,
            hint = "WiFi \u540D\u7A31"
        )
        Spacer(modifier = Modifier.height(8.dp))
        CyberTextField(
            value = values[passKey] ?: "",
            onValueChange = { onChange(passKey, it) },
            label = "\u5BC6\u78BC",
            leadingIcon = Icons.Default.VpnKey,
            isSecret = true,
            isModified = modified[passKey] == true,
            hint = "WiFi \u5BC6\u78BC"
        )
    }
}

// ═══════════════════════════════════════════════════════════
//  Bot Tab (Telegram)
// ═══════════════════════════════════════════════════════════

@Composable
fun BotTab(
    values: Map<String, String>,
    modified: Map<String, Boolean>,
    onChange: (String, String) -> Unit
) {
    val scope = rememberCoroutineScope()
    var tgTestState by remember { mutableStateOf<ApiTestState>(ApiTestState.Idle) }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        SectionLabel("TELEGRAM BOT")

        GlowCard {
            CyberTextField(
                value = values["tg_token"] ?: "",
                onValueChange = { onChange("tg_token", it) },
                label = "Bot Token",
                leadingIcon = Icons.Default.Send,
                isSecret = true,
                isModified = modified["tg_token"] == true,
                hint = "123456:ABC-DEF..."
            )

            val token = values["tg_token"] ?: ""
            if (token.isNotBlank()) {
                Spacer(modifier = Modifier.height(6.dp))
                Text(
                    "\u9810\u89BD: ${maskSecret(token, 6, 6)}",
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    color = TextDim
                )
            }

            Spacer(modifier = Modifier.height(12.dp))

            Row(verticalAlignment = Alignment.CenterVertically) {
                OutlinedButton(
                    onClick = {
                        tgTestState = ApiTestState.Testing
                        scope.launch {
                            val result = ApiTester.testTelegramToken(values["tg_token"] ?: "")
                            tgTestState = if (result.success) {
                                ApiTestState.Success(result.title, result.detail)
                            } else {
                                ApiTestState.Failed(result.title, result.detail)
                            }
                        }
                    },
                    shape = RoundedCornerShape(8.dp),
                    border = BorderStroke(1.dp, Cyan),
                    enabled = tgTestState !is ApiTestState.Testing,
                    contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp)
                ) {
                    when (tgTestState) {
                        is ApiTestState.Testing -> {
                            CircularProgressIndicator(modifier = Modifier.size(14.dp), strokeWidth = 2.dp, color = Cyan)
                            Spacer(modifier = Modifier.width(6.dp))
                            Text("\u6E2C\u8A66\u4E2D\u2026", fontSize = 12.sp, color = Cyan)
                        }
                        else -> {
                            Icon(Icons.Default.Science, null, modifier = Modifier.size(14.dp), tint = Cyan)
                            Spacer(modifier = Modifier.width(6.dp))
                            Text("\u6E2C\u8A66 Token", fontSize = 12.sp, color = Cyan)
                        }
                    }
                }
                Spacer(modifier = Modifier.width(8.dp))
                TestResultBadge(tgTestState)
            }
        }

        GlowCard {
            CyberTextField(
                value = values["chat_id"] ?: "",
                onValueChange = { onChange("chat_id", it) },
                label = "Chat ID",
                leadingIcon = Icons.Default.Person,
                isModified = modified["chat_id"] == true,
                hint = "\u4F60\u7684 Telegram \u7528\u6236 ID"
            )
        }

        Surface(
            modifier = Modifier.fillMaxWidth(),
            shape = RoundedCornerShape(12.dp),
            color = Purple.copy(alpha = 0.05f),
            border = BorderStroke(1.dp, Purple.copy(alpha = 0.1f))
        ) {
            Column(modifier = Modifier.padding(12.dp)) {
                Row {
                    Icon(Icons.Default.Info, null, tint = Purple, modifier = Modifier.size(14.dp))
                    Spacer(modifier = Modifier.width(6.dp))
                    Text("\u53D6\u5F97\u65B9\u5F0F", fontSize = 11.sp, fontWeight = FontWeight.Bold, color = Purple)
                }
                Spacer(modifier = Modifier.height(6.dp))
                Text(
                    "\u2022 Token: \u5728 Telegram \u627E @BotFather \u5EFA\u7ACB\n\u2022 Chat ID: \u767C\u9001 /start \u7D66 @userinfobot",
                    fontSize = 11.sp,
                    color = TextDim,
                    lineHeight = 16.sp
                )
            }
        }

        Spacer(modifier = Modifier.height(60.dp))
    }
}

// ═══════════════════════════════════════════════════════════
//  AI Tab (Gemini)
// ═══════════════════════════════════════════════════════════

@Composable
fun AiTab(
    values: Map<String, String>,
    modified: Map<String, Boolean>,
    onChange: (String, String) -> Unit
) {
    val scope = rememberCoroutineScope()
    var geminiTestState by remember { mutableStateOf<ApiTestState>(ApiTestState.Idle) }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        SectionLabel("GEMINI AI ENGINE")

        GlowCard {
            CyberTextField(
                value = values["gemini_key"] ?: "",
                onValueChange = { onChange("gemini_key", it) },
                label = "API Key",
                leadingIcon = Icons.Default.VpnKey,
                isSecret = true,
                isModified = modified["gemini_key"] == true,
                hint = "AIzaSy..."
            )

            val key = values["gemini_key"] ?: ""
            if (key.isNotBlank()) {
                Spacer(modifier = Modifier.height(6.dp))
                Text(
                    "\u9810\u89BD: ${maskSecret(key, 6, 4)}",
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    color = TextDim
                )
            }

            Spacer(modifier = Modifier.height(12.dp))

            Row(verticalAlignment = Alignment.CenterVertically) {
                OutlinedButton(
                    onClick = {
                        geminiTestState = ApiTestState.Testing
                        scope.launch {
                            val result = ApiTester.testGeminiKey(values["gemini_key"] ?: "")
                            geminiTestState = if (result.success) {
                                ApiTestState.Success(result.title, result.detail)
                            } else {
                                ApiTestState.Failed(result.title, result.detail)
                            }
                        }
                    },
                    shape = RoundedCornerShape(8.dp),
                    border = BorderStroke(1.dp, Cyan),
                    enabled = geminiTestState !is ApiTestState.Testing,
                    contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp)
                ) {
                    when (geminiTestState) {
                        is ApiTestState.Testing -> {
                            CircularProgressIndicator(modifier = Modifier.size(14.dp), strokeWidth = 2.dp, color = Cyan)
                            Spacer(modifier = Modifier.width(6.dp))
                            Text("\u6E2C\u8A66\u4E2D\u2026", fontSize = 12.sp, color = Cyan)
                        }
                        else -> {
                            Icon(Icons.Default.Science, null, modifier = Modifier.size(14.dp), tint = Cyan)
                            Spacer(modifier = Modifier.width(6.dp))
                            Text("\u6E2C\u8A66 API Key", fontSize = 12.sp, color = Cyan)
                        }
                    }
                }
                Spacer(modifier = Modifier.width(8.dp))
                TestResultBadge(geminiTestState)
            }
        }

        GlowCard {
            CyberTextField(
                value = values["model_pref"] ?: "",
                onValueChange = { onChange("model_pref", it) },
                label = "AI \u6A21\u578B",
                leadingIcon = Icons.Default.AutoAwesome,
                isModified = modified["model_pref"] == true,
                hint = "gemini-2.0-flash"
            )
            Spacer(modifier = Modifier.height(8.dp))
            Text("\u53EF\u7528\u6A21\u578B", fontSize = 11.sp, fontWeight = FontWeight.Bold, color = TextDim)
            Spacer(modifier = Modifier.height(4.dp))

            listOf(
                "gemini-3-flash-preview",
                "gemini-3-pro-preview",
                "gemini-2.0-flash",
                "gemini-1.5-pro",
                "gemini-1.5-flash"
            ).forEach { model ->
                Surface(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(vertical = 2.dp)
                        .clickable { onChange("model_pref", model) },
                    shape = RoundedCornerShape(8.dp),
                    color = if (values["model_pref"] == model) Cyan.copy(alpha = 0.1f) else Color.Transparent
                ) {
                    Text(
                        model,
                        modifier = Modifier.padding(horizontal = 8.dp, vertical = 6.dp),
                        fontSize = 12.sp,
                        fontFamily = FontFamily.Monospace,
                        color = if (values["model_pref"] == model) Cyan else TextDim
                    )
                }
            }
        }

        Spacer(modifier = Modifier.height(60.dp))
    }
}

// ═══════════════════════════════════════════════════════════
//  Camera Tab
// ═══════════════════════════════════════════════════════════

@Composable
fun CameraTab(
    values: Map<String, String>,
    modified: Map<String, Boolean>,
    onChange: (String, String) -> Unit
) {
    val camEnabled = values["cam_enable"] == "1"
    val camTask = values["cam_task"] ?: ""
    val camQuality = values["cam_quality"]?.toIntOrNull() ?: 80

    val taskPresets = listOf(
        "辨識物品" to "識別畫面中的物品並說明用途",
        "掃描收據" to "讀取收據金額與品項並整理",
        "掃描文字" to "辨識畫面中所有文字並回傳",
        "辨識食物" to "辨識食物種類並估算卡路里",
        "掃描名片" to "提取名片上的姓名、電話、email",
        "自訂" to ""
    )

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        SectionLabel("相機設定 (OV3660)")

        GlowCard {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically
            ) {
                Column {
                    Text("啟用相機模組", fontSize = 14.sp, color = TextMain, fontWeight = FontWeight.Medium)
                    Text("OV3660 / OV2640 DVP 介面", fontSize = 11.sp, color = TextDim)
                }
                Switch(
                    checked = camEnabled,
                    onCheckedChange = { onChange("cam_enable", if (it) "1" else "0") },
                    colors = SwitchDefaults.colors(
                        checkedThumbColor = BgDeep,
                        checkedTrackColor = Cyan,
                        uncheckedTrackColor = BorderDim
                    )
                )
            }

            if (camEnabled) {
                Spacer(modifier = Modifier.height(12.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(Icons.Default.Tune, null, tint = Cyan, modifier = Modifier.size(16.dp))
                    Spacer(modifier = Modifier.width(6.dp))
                    Text("JPEG 畫質: $camQuality%", fontSize = 12.sp, color = TextDim, fontFamily = FontFamily.Monospace)
                }
                Spacer(modifier = Modifier.height(4.dp))
                Slider(
                    value = camQuality.toFloat(),
                    onValueChange = { onChange("cam_quality", it.toInt().toString()) },
                    valueRange = 10f..95f,
                    steps = 16,
                    colors = SliderDefaults.colors(
                        thumbColor = Cyan,
                        activeTrackColor = Cyan,
                        inactiveTrackColor = BorderDim
                    )
                )
            }
        }

        if (camEnabled) {
            GlowCard {
                SectionLabel("拍照任務 Preset")
                Text("選擇 /snap 指令觸發後 AI 要做什麼分析", fontSize = 11.sp, color = TextDim)
                Spacer(modifier = Modifier.height(8.dp))

                taskPresets.forEach { (label, prompt) ->
                    val isSelected = if (label == "自訂") {
                        taskPresets.none { (l, p) -> l != "自訂" && p == camTask }
                    } else camTask == prompt

                    Surface(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(vertical = 3.dp)
                            .clickable { if (label != "自訂") onChange("cam_task", prompt) },
                        shape = RoundedCornerShape(10.dp),
                        color = if (isSelected) Cyan.copy(alpha = 0.12f) else BgInput,
                        border = androidx.compose.foundation.BorderStroke(
                            1.dp,
                            if (isSelected) Cyan.copy(alpha = 0.5f) else BorderDim
                        )
                    ) {
                        Row(
                            modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp),
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Box(
                                modifier = Modifier
                                    .size(8.dp)
                                    .clip(CircleShape)
                                    .background(if (isSelected) Cyan else BorderDim)
                            )
                            Spacer(modifier = Modifier.width(10.dp))
                            Column(modifier = Modifier.weight(1f)) {
                                Text(label, fontSize = 13.sp, color = if (isSelected) Cyan else TextMain, fontWeight = FontWeight.Medium)
                                if (prompt.isNotEmpty()) {
                                    Text(prompt, fontSize = 11.sp, color = TextDim, maxLines = 1, overflow = TextOverflow.Ellipsis)
                                }
                            }
                        }
                    }
                }

                Spacer(modifier = Modifier.height(8.dp))
                CyberTextField(
                    value = camTask,
                    onValueChange = { onChange("cam_task", it) },
                    label = "自訂 Prompt",
                    leadingIcon = Icons.Default.Edit,
                    isModified = modified["cam_task"] == true,
                    hint = "請分析圖片中的..."
                )
            }

            GlowCard {
                SectionLabel("快速指令")
                Text("在對話 Tab 輸入以下指令控制相機：", fontSize = 11.sp, color = TextDim)
                Spacer(modifier = Modifier.height(8.dp))
                listOf(
                    "/snap" to "立即拍照並用目前 Preset 分析",
                    "/snap 描述這張圖" to "拍照後用指定 Prompt 分析",
                    "/photo" to "/snap 的別名"
                ).forEach { (cmd, desc) ->
                    Row(modifier = Modifier.padding(vertical = 3.dp)) {
                        Text(cmd, fontSize = 12.sp, color = Cyan, fontFamily = FontFamily.Monospace, modifier = Modifier.width(180.dp))
                        Text(desc, fontSize = 11.sp, color = TextDim)
                    }
                }
            }
        }

        Spacer(modifier = Modifier.height(60.dp))
    }
}

// ═══════════════════════════════════════════════════════════
//  Setup Wizard (first-time onboarding)
// ═══════════════════════════════════════════════════════════

private enum class WizardStep { WELCOME, WIFI, TELEGRAM, GEMINI, DONE }

@Composable
fun SetupWizard(
    initialValues: Map<String, String>,
    onComplete: (Map<String, String>) -> Unit,
    onSkip: () -> Unit
) {
    val scope = rememberCoroutineScope()
    var step by remember { mutableStateOf(WizardStep.WELCOME) }
    val wiz = remember { mutableStateMapOf<String, String>().also { it.putAll(initialValues) } }

    var tgTestState by remember { mutableStateOf<ApiTestState>(ApiTestState.Idle) }
    var geminiTestState by remember { mutableStateOf<ApiTestState>(ApiTestState.Idle) }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(BgDeep.copy(alpha = 0.97f)),
        contentAlignment = Alignment.Center
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(24.dp)
                .verticalScroll(rememberScrollState()),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            // Progress dots
            if (step != WizardStep.WELCOME && step != WizardStep.DONE) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    listOf(WizardStep.WIFI, WizardStep.TELEGRAM, WizardStep.GEMINI).forEachIndexed { idx, s ->
                        val active = s == step
                        val done = step.ordinal > s.ordinal
                        Box(
                            modifier = Modifier
                                .size(if (active) 10.dp else 8.dp)
                                .clip(CircleShape)
                                .background(when { done -> NeonGreen; active -> Cyan; else -> BorderDim })
                        )
                    }
                }
                Spacer(modifier = Modifier.height(24.dp))
            }

            when (step) {
                // ── Welcome ──
                WizardStep.WELCOME -> {
                    Text("SERIAL SOUL", fontSize = 36.sp, fontWeight = FontWeight.ExtraBold,
                        fontFamily = FontFamily.Monospace, color = Cyan, letterSpacing = 6.sp)
                    Spacer(modifier = Modifier.height(8.dp))
                    Text("AI 助手設定精靈", fontSize = 16.sp, color = TextDim)
                    Spacer(modifier = Modifier.height(32.dp))

                    GlowCard {
                        listOf(
                            Icons.Default.Wifi to "設定 WiFi 讓裝置上網",
                            Icons.Default.Send to "連接 Telegram Bot 遠端控制",
                            Icons.Default.AutoAwesome to "設定 Gemini AI 啟動智慧功能"
                        ).forEach { (icon, desc) ->
                            Row(
                                modifier = Modifier.padding(vertical = 8.dp),
                                verticalAlignment = Alignment.CenterVertically
                            ) {
                                Icon(icon, null, tint = Cyan, modifier = Modifier.size(20.dp))
                                Spacer(modifier = Modifier.width(12.dp))
                                Text(desc, fontSize = 14.sp, color = TextMain)
                            }
                        }
                    }

                    Spacer(modifier = Modifier.height(24.dp))
                    Button(
                        onClick = { step = WizardStep.WIFI },
                        modifier = Modifier.fillMaxWidth(),
                        colors = ButtonDefaults.buttonColors(containerColor = Cyan),
                        shape = RoundedCornerShape(12.dp)
                    ) {
                        Text("開始設定", color = BgDeep, fontWeight = FontWeight.Bold, fontSize = 16.sp,
                            modifier = Modifier.padding(vertical = 6.dp))
                    }
                    Spacer(modifier = Modifier.height(12.dp))
                    TextButton(onClick = onSkip) {
                        Text("跳過（稍後手動設定）", color = TextDim, fontSize = 13.sp)
                    }
                }

                // ── WiFi ──
                WizardStep.WIFI -> {
                    Text("WiFi 設定", fontSize = 24.sp, fontWeight = FontWeight.Bold,
                        fontFamily = FontFamily.Monospace, color = Cyan)
                    Spacer(modifier = Modifier.height(6.dp))
                    Text("裝置需要 2.4GHz WiFi 才能上網", fontSize = 13.sp, color = TextDim)
                    Spacer(modifier = Modifier.height(20.dp))

                    GlowCard {
                        CyberTextField(
                            value = wiz["wifi_ssid"] ?: "",
                            onValueChange = { wiz["wifi_ssid"] = it },
                            label = "WiFi 名稱 (SSID)",
                            leadingIcon = Icons.Default.Wifi,
                            hint = "MyHome-WiFi"
                        )
                        Spacer(modifier = Modifier.height(12.dp))
                        CyberTextField(
                            value = wiz["wifi_pass"] ?: "",
                            onValueChange = { wiz["wifi_pass"] = it },
                            label = "WiFi 密碼",
                            leadingIcon = Icons.Default.Lock,
                            isSecret = true,
                            hint = "至少 8 個字元"
                        )
                        Spacer(modifier = Modifier.height(8.dp))
                        Text("⚠ 僅支援 2.4GHz，不支援 5GHz", fontSize = 11.sp, color = NeonAmber)
                    }

                    Spacer(modifier = Modifier.height(20.dp))
                    Button(
                        onClick = { step = WizardStep.TELEGRAM },
                        enabled = (wiz["wifi_ssid"]?.isNotBlank() == true),
                        modifier = Modifier.fillMaxWidth(),
                        colors = ButtonDefaults.buttonColors(containerColor = Cyan),
                        shape = RoundedCornerShape(12.dp)
                    ) {
                        Text("下一步：Telegram Bot", color = BgDeep, fontWeight = FontWeight.Bold,
                            modifier = Modifier.padding(vertical = 6.dp))
                    }
                }

                // ── Telegram ──
                WizardStep.TELEGRAM -> {
                    Text("Telegram Bot", fontSize = 24.sp, fontWeight = FontWeight.Bold,
                        fontFamily = FontFamily.Monospace, color = Cyan)
                    Spacer(modifier = Modifier.height(6.dp))
                    Text("透過 Telegram 遠端控制與接收通知", fontSize = 13.sp, color = TextDim)
                    Spacer(modifier = Modifier.height(20.dp))

                    GlowCard {
                        CyberTextField(
                            value = wiz["tg_token"] ?: "",
                            onValueChange = { wiz["tg_token"] = it; tgTestState = ApiTestState.Idle },
                            label = "Bot Token",
                            leadingIcon = Icons.Default.Send,
                            isSecret = true,
                            hint = "1234567890:ABC..."
                        )
                        Spacer(modifier = Modifier.height(4.dp))
                        Text("從 @BotFather → /newbot 取得", fontSize = 11.sp, color = TextDim)
                        Spacer(modifier = Modifier.height(12.dp))
                        CyberTextField(
                            value = wiz["chat_id"] ?: "",
                            onValueChange = { wiz["chat_id"] = it },
                            label = "Chat ID（你的 Telegram ID）",
                            leadingIcon = Icons.Default.Person,
                            hint = "從 @userinfobot 取得"
                        )
                        Spacer(modifier = Modifier.height(12.dp))
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            OutlinedButton(
                                onClick = {
                                    tgTestState = ApiTestState.Testing
                                    scope.launch {
                                        val r = ApiTester.testTelegramToken(wiz["tg_token"] ?: "")
                                        tgTestState = if (r.success) ApiTestState.Success(r.title, r.detail)
                                        else ApiTestState.Failed(r.title, r.detail)
                                    }
                                },
                                enabled = tgTestState !is ApiTestState.Testing && !wiz["tg_token"].isNullOrBlank(),
                                shape = RoundedCornerShape(8.dp),
                                border = BorderStroke(1.dp, Cyan),
                                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp)
                            ) {
                                if (tgTestState is ApiTestState.Testing) {
                                    CircularProgressIndicator(modifier = Modifier.size(14.dp), strokeWidth = 2.dp, color = Cyan)
                                    Spacer(modifier = Modifier.width(6.dp))
                                    Text("測試中…", fontSize = 12.sp, color = Cyan)
                                } else {
                                    Icon(Icons.Default.Science, null, modifier = Modifier.size(14.dp), tint = Cyan)
                                    Spacer(modifier = Modifier.width(6.dp))
                                    Text("測試 Token", fontSize = 12.sp, color = Cyan)
                                }
                            }
                            Spacer(modifier = Modifier.width(8.dp))
                            TestResultBadge(tgTestState)
                        }
                    }

                    Spacer(modifier = Modifier.height(20.dp))
                    Button(
                        onClick = { step = WizardStep.GEMINI },
                        enabled = !wiz["tg_token"].isNullOrBlank(),
                        modifier = Modifier.fillMaxWidth(),
                        colors = ButtonDefaults.buttonColors(containerColor = Cyan),
                        shape = RoundedCornerShape(12.dp)
                    ) {
                        Text("下一步：Gemini AI", color = BgDeep, fontWeight = FontWeight.Bold,
                            modifier = Modifier.padding(vertical = 6.dp))
                    }
                    Spacer(modifier = Modifier.height(8.dp))
                    TextButton(onClick = { step = WizardStep.WIFI }) {
                        Text("← 上一步", color = TextDim, fontSize = 13.sp)
                    }
                }

                // ── Gemini ──
                WizardStep.GEMINI -> {
                    Text("Gemini AI 設定", fontSize = 24.sp, fontWeight = FontWeight.Bold,
                        fontFamily = FontFamily.Monospace, color = Cyan)
                    Spacer(modifier = Modifier.height(6.dp))
                    Text("免費取得 Gemini API Key 啟動 AI 功能", fontSize = 13.sp, color = TextDim)
                    Spacer(modifier = Modifier.height(20.dp))

                    GlowCard {
                        CyberTextField(
                            value = wiz["gemini_key"] ?: "",
                            onValueChange = { wiz["gemini_key"] = it; geminiTestState = ApiTestState.Idle },
                            label = "Gemini API Key",
                            leadingIcon = Icons.Default.VpnKey,
                            isSecret = true,
                            hint = "AIzaSy..."
                        )
                        Spacer(modifier = Modifier.height(4.dp))
                        Text("從 aistudio.google.com/apikey 免費取得", fontSize = 11.sp, color = TextDim)
                        Spacer(modifier = Modifier.height(12.dp))
                        Text("選擇 AI 模型", fontSize = 12.sp, color = TextDim, fontWeight = FontWeight.Bold)
                        Spacer(modifier = Modifier.height(6.dp))
                        listOf("gemini-3-flash-preview" to "⚡ 最新快速版（預設）",
                               "gemini-3-pro-preview" to "🧠 最新高品質版",
                               "gemini-2.0-flash" to "穩定快速版").forEach { (model, desc) ->
                            Surface(
                                modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)
                                    .clickable { wiz["model_pref"] = model },
                                shape = RoundedCornerShape(8.dp),
                                color = if (wiz["model_pref"] == model || (wiz["model_pref"].isNullOrBlank() && model == "gemini-3-flash-preview"))
                                    Cyan.copy(alpha = 0.12f) else Color.Transparent
                            ) {
                                Row(modifier = Modifier.padding(8.dp), verticalAlignment = Alignment.CenterVertically) {
                                    Text(model, fontSize = 12.sp, fontFamily = FontFamily.Monospace,
                                        color = if (wiz["model_pref"] == model) Cyan else TextMain,
                                        modifier = Modifier.weight(1f))
                                    Text(desc, fontSize = 11.sp, color = TextDim)
                                }
                            }
                        }
                        Spacer(modifier = Modifier.height(12.dp))
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            OutlinedButton(
                                onClick = {
                                    geminiTestState = ApiTestState.Testing
                                    scope.launch {
                                        val r = ApiTester.testGeminiKey(wiz["gemini_key"] ?: "")
                                        geminiTestState = if (r.success) ApiTestState.Success(r.title, r.detail)
                                        else ApiTestState.Failed(r.title, r.detail)
                                    }
                                },
                                enabled = geminiTestState !is ApiTestState.Testing && !wiz["gemini_key"].isNullOrBlank(),
                                shape = RoundedCornerShape(8.dp),
                                border = BorderStroke(1.dp, Cyan),
                                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 6.dp)
                            ) {
                                if (geminiTestState is ApiTestState.Testing) {
                                    CircularProgressIndicator(modifier = Modifier.size(14.dp), strokeWidth = 2.dp, color = Cyan)
                                    Spacer(modifier = Modifier.width(6.dp))
                                    Text("測試中…", fontSize = 12.sp, color = Cyan)
                                } else {
                                    Icon(Icons.Default.Science, null, modifier = Modifier.size(14.dp), tint = Cyan)
                                    Spacer(modifier = Modifier.width(6.dp))
                                    Text("測試 API Key", fontSize = 12.sp, color = Cyan)
                                }
                            }
                            Spacer(modifier = Modifier.width(8.dp))
                            TestResultBadge(geminiTestState)
                        }
                    }

                    Spacer(modifier = Modifier.height(20.dp))
                    Button(
                        onClick = {
                            if (wiz["model_pref"].isNullOrBlank()) wiz["model_pref"] = "gemini-3-flash-preview"
                            onComplete(wiz.toMap())
                        },
                        enabled = !wiz["gemini_key"].isNullOrBlank(),
                        modifier = Modifier.fillMaxWidth(),
                        colors = ButtonDefaults.buttonColors(containerColor = NeonGreen),
                        shape = RoundedCornerShape(12.dp)
                    ) {
                        Icon(Icons.Default.Save, null, tint = BgDeep, modifier = Modifier.size(18.dp))
                        Spacer(modifier = Modifier.width(8.dp))
                        Text("儲存並重啟裝置", color = BgDeep, fontWeight = FontWeight.Bold, fontSize = 16.sp,
                            modifier = Modifier.padding(vertical = 6.dp))
                    }
                    Spacer(modifier = Modifier.height(8.dp))
                    TextButton(onClick = { step = WizardStep.TELEGRAM }) {
                        Text("← 上一步", color = TextDim, fontSize = 13.sp)
                    }
                }

                WizardStep.DONE -> {}
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
//  System Tab
// ═══════════════════════════════════════════════════════════

@Composable
fun SystemTab(
    values: Map<String, String>,
    modified: Map<String, Boolean>,
    fwVersion: String,
    deviceName: String = "",
    onChange: (String, String) -> Unit
) {
    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        SectionLabel("SYSTEM SETTINGS")

        GlowCard {
            SectionLabel("\u8A9E\u97F3\u63A7\u5236")
            CyberTextField(
                value = values["voice_mode"] ?: "",
                onValueChange = { onChange("voice_mode", it) },
                label = "\u8A9E\u97F3\u6A21\u5F0F",
                leadingIcon = Icons.Default.Mic,
                isModified = modified["voice_mode"] == true,
                hint = "tts / input / full / off"
            )
            Spacer(modifier = Modifier.height(8.dp))
            CyberTextField(
                value = values["wake_phrase"] ?: "",
                onValueChange = { onChange("wake_phrase", it) },
                label = "\u559A\u9192\u8A5E",
                leadingIcon = Icons.Default.RecordVoiceOver,
                isModified = modified["wake_phrase"] == true,
                hint = "\u5982: ethan"
            )
        }

        GlowCard {
            SectionLabel("\u5B89\u5168\u8A2D\u5B9A")
            CyberTextField(
                value = values["ble_pin"] ?: "",
                onValueChange = { onChange("ble_pin", it) },
                label = "BLE \u9023\u7DDA\u5BC6\u78BC",
                leadingIcon = Icons.Default.Lock,
                isSecret = true,
                isModified = modified["ble_pin"] == true,
                hint = "\u81F3\u5C11 6 \u5B57\u5143"
            )
        }

        GlowCard {
            SectionLabel("\u88DD\u7F6E\u8CC7\u8A0A")
            InfoRow("\u97CC\u9AD4\u7248\u672C", fwVersion.ifEmpty { "\u672A\u77E5" })
            InfoRow("BLE \u540D\u7A31", deviceName.ifEmpty { "SerialSoul-XXXX" })
            InfoRow("\u901A\u8A0A\u5354\u8B70", "Nordic UART Service (NUS)")
        }

        Spacer(modifier = Modifier.height(60.dp))
    }
}

@Composable
fun InfoRow(label: String, value: String) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 4.dp),
        horizontalArrangement = Arrangement.SpaceBetween
    ) {
        Text(label, fontSize = 12.sp, color = TextDim)
        Text(
            value,
            fontSize = 12.sp,
            fontFamily = FontFamily.Monospace,
            color = Cyan,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis
        )
    }
}

// ═══════════════════════════════════════════════════════════
//  Log Tab
// ═══════════════════════════════════════════════════════════

@Composable
fun LogTab(logLines: List<String>) {
    val listState = rememberLazyListState()

    LaunchedEffect(logLines.size) {
        if (logLines.isNotEmpty()) listState.animateScrollToItem(logLines.lastIndex)
    }

    Column(modifier = Modifier.fillMaxSize()) {
        Surface(modifier = Modifier.fillMaxWidth(), color = BgCard) {
            Row(
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically
            ) {
                Icon(Icons.Default.Code, null, tint = Cyan, modifier = Modifier.size(14.dp))
                Spacer(modifier = Modifier.width(8.dp))
                Text(
                    "BLE COMMUNICATION LOG",
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    color = TextDim,
                    letterSpacing = 1.sp
                )
                Spacer(modifier = Modifier.weight(1f))
                Text("${logLines.size} entries", fontSize = 10.sp, fontFamily = FontFamily.Monospace, color = TextDim)
            }
        }

        LazyColumn(
            state = listState,
            modifier = Modifier
                .fillMaxSize()
                .background(Color(0xFF050810))
                .padding(horizontal = 12.dp, vertical = 8.dp)
        ) {
            items(logLines) { line ->
                Text(
                    line,
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                    color = when {
                        line.startsWith("\u2190") -> NeonGreen
                        line.startsWith("\u2192") -> Cyan
                        line.contains("\u932F\u8AA4") || line.contains("\u5931\u6557") || line.contains("FAIL") -> NeonRed
                        line.contains("\u6210\u529F") || line.contains("OK") -> NeonGreen
                        else -> TextDim.copy(alpha = 0.7f)
                    },
                    modifier = Modifier.padding(vertical = 1.dp),
                    lineHeight = 16.sp
                )
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
//  Test Result Badge
// ═══════════════════════════════════════════════════════════

@Composable
fun TestResultBadge(state: ApiTestState) {
    when (state) {
        is ApiTestState.Idle, is ApiTestState.Testing -> {}
        is ApiTestState.Success -> {
            Surface(
                shape = RoundedCornerShape(8.dp),
                color = NeonGreen.copy(alpha = 0.1f),
                border = BorderStroke(1.dp, NeonGreen.copy(alpha = 0.3f))
            ) {
                Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(Icons.Default.CheckCircle, null, tint = NeonGreen, modifier = Modifier.size(14.dp))
                        Spacer(modifier = Modifier.width(4.dp))
                        Text(state.title, fontSize = 11.sp, fontWeight = FontWeight.Bold, color = NeonGreen)
                    }
                    if (state.detail.isNotBlank()) {
                        Text(state.detail, fontSize = 10.sp, color = NeonGreen.copy(alpha = 0.7f))
                    }
                }
            }
        }
        is ApiTestState.Failed -> {
            Surface(
                shape = RoundedCornerShape(8.dp),
                color = NeonRed.copy(alpha = 0.1f),
                border = BorderStroke(1.dp, NeonRed.copy(alpha = 0.3f))
            ) {
                Column(modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(Icons.Default.Error, null, tint = NeonRed, modifier = Modifier.size(14.dp))
                        Spacer(modifier = Modifier.width(4.dp))
                        Text(state.title, fontSize = 11.sp, fontWeight = FontWeight.Bold, color = NeonRed)
                    }
                    if (state.detail.isNotBlank()) {
                        Text(state.detail, fontSize = 10.sp, color = NeonRed.copy(alpha = 0.7f))
                    }
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════
//  Demo Config Screen (UI preview without BLE)
// ═══════════════════════════════════════════════════════════

@Composable
fun DemoConfigScreen(onExit: () -> Unit) {
    var selectedTab by remember { mutableStateOf(ConfigTab.CHAT) }
    var statusMsg by remember { mutableStateOf("") }
    var statusOk by remember { mutableStateOf(true) }
    val values = remember {
        mutableStateMapOf(
            "wifi_ssid" to "MyHomeWiFi", "wifi_pass" to "password123",
            "wifi_ssid2" to "OfficeNet", "wifi_pass2" to "office456",
            "wifi_ssid3" to "", "wifi_pass3" to "",
            "tg_token" to "",
            "chat_id" to "",
            "gemini_key" to "",
            "model_pref" to "gemini-3-flash-preview",
            "voice_mode" to "tts", "wake_phrase" to "ethan", "ble_pin" to "1234",
            "cam_enable" to "1", "cam_task" to "辨識物品並說明用途", "cam_quality" to "80"
        )
    }
    val modified = remember { mutableStateMapOf<String, Boolean>() }
    val demoLog = remember {
        listOf("\u2192 PING", "\u2190 PONG SerialSoul v1.0.0", "\u2192 GET wifi_ssid",
            "\u2190 OK MyHomeWiFi", "\u2192 GET tg_token", "\u2190 OK 8763****EvpoQ",
            "\u2192 GET gemini_key", "\u2190 OK AIza****SlC", "\u8A8D\u8B49\u6210\u529F \u2713")
    }

    Column(modifier = Modifier.fillMaxSize().background(BgDeep)) {
        Surface(color = BgCard, tonalElevation = 0.dp) {
            Column {
                Row(modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp), verticalAlignment = Alignment.CenterVertically) {
                    Box(modifier = Modifier.size(36.dp).clip(RoundedCornerShape(10.dp)).background(Cyan.copy(alpha = 0.1f)), contentAlignment = Alignment.Center) {
                        Icon(Icons.Default.Bluetooth, null, tint = Cyan, modifier = Modifier.size(20.dp))
                    }
                    Spacer(modifier = Modifier.width(12.dp))
                    Column(modifier = Modifier.weight(1f)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text("SERIAL SOUL", fontWeight = FontWeight.Bold, fontFamily = FontFamily.Monospace, fontSize = 16.sp, color = Cyan, letterSpacing = 2.sp)
                            Spacer(modifier = Modifier.width(8.dp))
                            Surface(shape = RoundedCornerShape(4.dp), color = NeonAmber.copy(alpha = 0.15f)) {
                                Text("DEMO", modifier = Modifier.padding(horizontal = 6.dp, vertical = 1.dp), fontSize = 9.sp, fontWeight = FontWeight.Bold, fontFamily = FontFamily.Monospace, color = NeonAmber)
                            }
                        }
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Box(modifier = Modifier.size(6.dp).clip(CircleShape).background(NeonGreen))
                            Spacer(modifier = Modifier.width(6.dp))
                            Text("PONG SerialSoul v1.0.0", fontSize = 11.sp, fontFamily = FontFamily.Monospace, color = TextDim)
                        }
                    }
                    IconButton(onClick = onExit) { Icon(Icons.Default.PowerSettingsNew, null, tint = NeonRed, modifier = Modifier.size(22.dp)) }
                }
                ScrollableTabRow(
                    selectedTabIndex = ConfigTab.values().indexOf(selectedTab), containerColor = Color.Transparent, contentColor = Cyan, edgePadding = 8.dp,
                    indicator = { tabPositions ->
                        val idx = ConfigTab.values().indexOf(selectedTab)
                        if (idx in tabPositions.indices) { with(TabRowDefaults) { Indicator(modifier = Modifier.tabIndicatorOffset(tabPositions[idx]), height = 2.dp, color = Cyan) } }
                    },
                    divider = { Divider(color = BorderDim, thickness = 1.dp) }
                ) {
                    ConfigTab.values().forEach { tab ->
                        Tab(selected = selectedTab == tab, onClick = { selectedTab = tab },
                            text = { Text(tab.label, fontSize = 12.sp, fontWeight = if (selectedTab == tab) FontWeight.Bold else FontWeight.Normal, fontFamily = FontFamily.Monospace) },
                            icon = { Icon(tab.icon, null, modifier = Modifier.size(18.dp), tint = if (selectedTab == tab) Cyan else TextDim) },
                            selectedContentColor = Cyan, unselectedContentColor = TextDim)
                    }
                }
            }
        }
        AnimatedVisibility(visible = statusMsg.isNotEmpty()) {
            Surface(modifier = Modifier.fillMaxWidth(), color = if (statusOk) NeonGreen.copy(alpha = 0.1f) else NeonRed.copy(alpha = 0.1f)) {
                Row(modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                    Icon(if (statusOk) Icons.Default.CheckCircle else Icons.Default.Error, null, tint = if (statusOk) NeonGreen else NeonRed, modifier = Modifier.size(16.dp))
                    Spacer(modifier = Modifier.width(8.dp))
                    Text(statusMsg, fontSize = 12.sp, color = if (statusOk) NeonGreen else NeonRed)
                }
            }
        }
        Box(modifier = Modifier.weight(1f)) {
            when (selectedTab) {
                ConfigTab.CHAT   -> ChatTab(
                    sendMessage = { msg ->
                        kotlinx.coroutines.delay(1500L)
                        when {
                            msg.startsWith("/help", ignoreCase = true) -> "OK 可用指令:\n/status — 裝置狀態\n/snap — 拍照\n/wifi — WiFi 資訊\n/help — 指令說明\n\n或直接輸入文字與 AI 對話"
                            msg.startsWith("/snap", ignoreCase = true) -> "OK 📷 Demo 模式無法拍照"
                            msg.startsWith("/wifi", ignoreCase = true) -> "OK 🌐 WiFi: MyHomeWiFi (已連線)\nIP: 192.168.1.42\nRSSI: -52 dBm"
                            else -> "OK 這是 Demo 模式的模擬回應。連線 ESP32 後，此訊息將由 Gemini AI 回覆。\n\n你說：「$msg」"
                        }
                    },
                    getStatus = {
                        kotlinx.coroutines.delay(500L)
                        "OK heap=125KB up=2h15m fw=v1.0.0"
                    }
                )
                ConfigTab.WIFI   -> WifiTab(values, modified) { k, v -> values[k] = v; modified[k] = true }
                ConfigTab.BOT    -> BotTab(values, modified) { k, v -> values[k] = v; modified[k] = true }
                ConfigTab.AI     -> AiTab(values, modified) { k, v -> values[k] = v; modified[k] = true }
                ConfigTab.CAMERA -> CameraTab(values, modified) { k, v -> values[k] = v; modified[k] = true }
                ConfigTab.SYSTEM -> SystemTab(values, modified, "PONG SerialSoul v1.0.0") { k, v -> values[k] = v; modified[k] = true }
                ConfigTab.LOG    -> LogTab(demoLog)
            }
        }
        if (selectedTab != ConfigTab.CHAT) {
        Surface(color = BgCard) {
            Column {
                Divider(color = BorderDim, thickness = 1.dp)
                Row(modifier = Modifier.fillMaxWidth().padding(12.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    OutlinedButton(onClick = { statusMsg = "\u5DF2\u91CD\u65B0\u8B80\u53D6 (Demo)"; statusOk = true; modified.clear() },
                        modifier = Modifier.weight(1f).height(44.dp), shape = RoundedCornerShape(12.dp), border = BorderStroke(1.dp, BorderDim)) {
                        Icon(Icons.Default.Refresh, null, modifier = Modifier.size(16.dp), tint = TextDim); Spacer(modifier = Modifier.width(6.dp)); Text("\u91CD\u8B80", color = TextDim, fontSize = 13.sp)
                    }
                    Button(onClick = { statusMsg = "\u5DF2\u5BEB\u5165 (Demo)"; statusOk = true; modified.clear() },
                        modifier = Modifier.weight(1f).height(44.dp), shape = RoundedCornerShape(12.dp), enabled = modified.any { it.value },
                        colors = ButtonDefaults.buttonColors(containerColor = NeonGreen.copy(alpha = 0.15f), disabledContainerColor = BgInput),
                        border = BorderStroke(1.dp, if (modified.any { it.value }) NeonGreen else BorderDim)) {
                        Icon(Icons.Default.Save, null, modifier = Modifier.size(16.dp), tint = if (modified.any { it.value }) NeonGreen else TextDim); Spacer(modifier = Modifier.width(6.dp))
                        Text("\u5132\u5B58\u4E26\u91CD\u555F", color = if (modified.any { it.value }) NeonGreen else TextDim, fontSize = 13.sp)
                    }
                }
            }
        }
        } // end if (selectedTab != ConfigTab.CHAT)
    }
}
