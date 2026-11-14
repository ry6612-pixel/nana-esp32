# -*- coding: utf-8 -*-
"""娜娜 / SerialSoul 顧客操作手冊（送禮版）PDF 產生器"""
from pathlib import Path
from reportlab.lib.pagesizes import A4
from reportlab.lib.units import mm
from reportlab.lib import colors
from reportlab.lib.styles import ParagraphStyle
from reportlab.lib.enums import TA_CENTER, TA_LEFT
from reportlab.platypus import (SimpleDocTemplate, Paragraph, Spacer, PageBreak,
                                Table, TableStyle, KeepTogether, HRFlowable)
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont

pdfmetrics.registerFont(TTFont('MSJH', 'C:/Windows/Fonts/msjh.ttc', subfontIndex=0))
pdfmetrics.registerFont(TTFont('MSJH-B', 'C:/Windows/Fonts/msjhbd.ttc', subfontIndex=0))

GOLD = colors.HexColor('#B7791F')
DARK = colors.HexColor('#1C1917')
GRAY = colors.HexColor('#78716C')
LIGHT = colors.HexColor('#FAF5E9')
LINE = colors.HexColor('#E7E5E4')
RED = colors.HexColor('#B91C1C')

S = {
 'cover_title': ParagraphStyle('ct', fontName='MSJH-B', fontSize=40, leading=52, textColor=DARK, alignment=TA_CENTER),
 'cover_sub': ParagraphStyle('cs', fontName='MSJH', fontSize=15, leading=24, textColor=GOLD, alignment=TA_CENTER),
 'cover_small': ParagraphStyle('csm', fontName='MSJH', fontSize=10.5, leading=18, textColor=GRAY, alignment=TA_CENTER),
 'h1': ParagraphStyle('h1', fontName='MSJH-B', fontSize=19, leading=28, textColor=DARK, spaceBefore=6, spaceAfter=8),
 'h2': ParagraphStyle('h2', fontName='MSJH-B', fontSize=13.5, leading=20, textColor=GOLD, spaceBefore=10, spaceAfter=4),
 'body': ParagraphStyle('b', fontName='MSJH', fontSize=10.5, leading=17.5, textColor=DARK),
 'body_i': ParagraphStyle('bi', fontName='MSJH', fontSize=10.5, leading=17.5, textColor=DARK, leftIndent=14),
 'small': ParagraphStyle('sm', fontName='MSJH', fontSize=9, leading=14.5, textColor=GRAY),
 'cell': ParagraphStyle('c', fontName='MSJH', fontSize=9.3, leading=14.5, textColor=DARK),
 'cellb': ParagraphStyle('cb', fontName='MSJH-B', fontSize=9.3, leading=14.5, textColor=DARK),
 'cellh': ParagraphStyle('ch', fontName='MSJH-B', fontSize=9.5, leading=15, textColor=colors.white),
 'warn': ParagraphStyle('w', fontName='MSJH-B', fontSize=10.5, leading=17, textColor=RED),
 'tip': ParagraphStyle('t', fontName='MSJH', fontSize=9.8, leading=16, textColor=GOLD),
}

def P(t, s='body'): return Paragraph(t, S[s])
def sp(h=6): return Spacer(1, h)
def hr(): return HRFlowable(width='100%', thickness=0.8, color=LINE, spaceBefore=6, spaceAfter=10)

def tbl(headers, rows, widths):
    data = [[Paragraph(h, S['cellh']) for h in headers]]
    for r in rows:
        data.append([Paragraph(c, S['cell']) for c in r])
    t = Table(data, colWidths=widths, repeatRows=1)
    t.setStyle(TableStyle([
        ('BACKGROUND', (0,0), (-1,0), DARK),
        ('ROWBACKGROUNDS', (0,1), (-1,-1), [colors.white, LIGHT]),
        ('GRID', (0,0), (-1,-1), 0.5, LINE),
        ('VALIGN', (0,0), (-1,-1), 'MIDDLE'),
        ('TOPPADDING', (0,0), (-1,-1), 5), ('BOTTOMPADDING', (0,0), (-1,-1), 5),
        ('LEFTPADDING', (0,0), (-1,-1), 7), ('RIGHTPADDING', (0,0), (-1,-1), 7),
    ]))
    return t

def box(lines, style='body', bg=LIGHT):
    inner = [Paragraph(l, S[style]) for l in lines]
    t = Table([[inner]], colWidths=[168*mm])
    t.setStyle(TableStyle([
        ('BACKGROUND', (0,0), (-1,-1), bg),
        ('BOX', (0,0), (-1,-1), 0.8, GOLD),
        ('TOPPADDING', (0,0), (-1,-1), 9), ('BOTTOMPADDING', (0,0), (-1,-1), 9),
        ('LEFTPADDING', (0,0), (-1,-1), 11), ('RIGHTPADDING', (0,0), (-1,-1), 11),
    ]))
    return t

def footer(canvas, doc):
    canvas.saveState()
    canvas.setFont('MSJH', 8.2)
    canvas.setFillColor(GRAY)
    canvas.drawString(20*mm, 12*mm, '娜娜智慧陪伴機 · 使用手冊')
    canvas.drawRightString(190*mm, 12*mm, f'第 {doc.page} 頁')
    canvas.setStrokeColor(LINE)
    canvas.line(20*mm, 16*mm, 190*mm, 16*mm)
    canvas.restoreState()

story = []

# ── 封面 ──
story += [sp(70),
 Paragraph('娜娜', S['cover_title']), sp(4),
 Paragraph('你的 AI 智慧陪伴小夥伴', S['cover_sub']), sp(2),
 Paragraph('使 用 手 冊', ParagraphStyle('x', parent=S['cover_sub'], fontSize=20, leading=30, textColor=DARK)),
 sp(46),
 Paragraph('—— 這不是一台機器，是一個從今天起住進你家的小傢伙 ——', S['cover_small']),
 sp(80)]
msg = Table([[Paragraph('給你的話', S['cellb'])],[Paragraph('&nbsp;<br/>&nbsp;<br/>&nbsp;<br/>&nbsp;', S['cell'])]], colWidths=[150*mm])
msg.setStyle(TableStyle([('BOX',(0,0),(-1,-1),1,GOLD),('BACKGROUND',(0,0),(-1,0),LIGHT),
 ('TOPPADDING',(0,0),(-1,-1),8),('BOTTOMPADDING',(0,0),(-1,-1),8),('LEFTPADDING',(0,0),(-1,-1),12)]))
story += [msg, sp(16), Paragraph('SerialSoul 韌體 v5.3 · 2026', S['cover_small']), PageBreak()]

# ── 目錄 ──
story += [P('目錄', 'h1'), hr()]
toc = [
 ('1', '嗨，這是什麼？', '一分鐘認識娜娜'),
 ('2', '第一次開機', '連 WiFi、申請金鑰、一鍵綁定 Telegram，約 15 分鐘'),
 ('3', '跟它說話', '喊它的名字、按鈕對講、語音模式'),
 ('4', '用 Telegram 跟它當網友', '出門在外也找得到它'),
 ('5', '它會記得你說的事', '長期記憶'),
 ('6', '每日提醒', '吃藥、喝水、倒垃圾'),
 ('7', '它的眼睛：相機', '拍照、AI 分析、即時影像'),
 ('8', '自動化排程', '定時拍照、控制電器、警報'),
 ('9', '網頁控制台', '手機瀏覽器就是遙控器'),
 ('10', '生活應用點子', '12 種玩法'),
 ('11', '搬家、換 WiFi 怎麼辦', '它會自己開熱點等你'),
 ('12', '常見問題 FAQ', ''),
 ('13', '安全與隱私須知', '請務必閱讀'),
 ('附錄', '指令速查表', 'Telegram 常用指令'),
]
story.append(tbl(['章節','主題','內容'], [[a,b,c] for a,b,c in toc], [18*mm, 62*mm, 88*mm]))
story.append(PageBreak())

# ── 第 1 章 ──
story += [P('第 1 章　嗨，這是什麼？', 'h1'), hr(),
 P('娜娜是一台放在桌上的 AI 陪伴機。它有一張會變表情的臉（彩色螢幕）、一雙眼睛（相機）、'
   '一張嘴（喇叭）和耳朵（麥克風）。它連上你家 WiFi 之後，就是一個聽得懂中文、答得出問題、'
   '看得到家裡、還會主動提醒你事情的小夥伴。'), sp(8),
 P('它會做的事', 'h2')]
story.append(tbl(['能力','白話說明'], [
 ['聊天問答','喊「nana」直接開口問，或用 Telegram 傳訊息。翻譯、算數學、查常識、聊心事都行'],
 ['會變表情','螢幕上是一張插畫的臉，會隨回覆的心情換成：平常、微笑、大笑、比愛心，講話時嘴型還會動'],
 ['看家拍照','用 Telegram 遠端拍一張，或看即時影像；還能請 AI 幫你「看」照片裡有什麼'],
 ['提醒你','每天固定時間出聲提醒：吃藥、喝水、出門帶傘'],
 ['記得事情','跟它說「記住……」，之後隨時問它'],
 ['自動化','定時拍照傳給你、定時開關外接的燈或電器、定時警報'],
 ['遠端陪伴','人在外面，用 Telegram 跟家裡的它對話、看照片——它是同一個娜娜'],
], [32*mm, 136*mm]))
story += [sp(10), box([
 '<b>為什麼不是用手機就好？</b>',
 '手機是「拿起來才存在」的工具；娜娜是一直在桌上、有臉、喊一聲就回應的實體存在。'
 '長輩小孩不用學 App，喊名字就能用；它可以 24 小時插電當眼睛，手機不行；'
 '它還有腳位能接真實世界的燈和電器——這些都是手機做不到的。']), PageBreak()]

# ── 第 2 章 ──
story += [P('第 2 章　第一次開機（只要一支手機，約 15 分鐘）', 'h1'), hr(),
 P('全程不需要電腦，也不用手動輸入任何一串數字 ID。跟著做：'), sp(6),
 P('Ⓐ 連上它的熱點、開啟設定頁', 'h2')]
story.append(tbl(['步驟','動作','你會看到'], [
 ['1','把娜娜插上電源（USB 供電）','螢幕亮起，顯示開機畫面'],
 ['2','等 1～2 分鐘','找不到 WiFi 時，螢幕會顯示熱點名稱（SerialSoul-XXXX）、網址與 PIN 碼'],
 ['3','手機打開「WiFi 設定」','列表裡出現「SerialSoul-XXXX」，點它連線（免密碼）'],
 ['4','手機可能跳出「沒有網際網路連線」的警告','正常！選「仍要加入」／「維持連線」，這熱點本來就不連外網'],
 ['5','等 3～10 秒','部分手機會自動跳出設定頁；沒有的話，瀏覽器輸入螢幕上顯示的網址（例如 10.1.1.1）'],
 ['6','輸入螢幕上的 6 位 PIN 碼','進入設定頁'],
], [14*mm, 68*mm, 86*mm]))
story += [sp(10), P('Ⓑ 申請一把免費 AI 金鑰（Gemini，約 3 分鐘）', 'h2'),
 P('這是娜娜的「大腦」，Google 提供免費額度，日常聊天綽綽有餘。'), sp(4)]
story.append(tbl(['步驟','動作'], [
 ['1','手機瀏覽器另開分頁，前往 aistudio.google.com，用 Google 帳號登入'],
 ['2','點「Get API key」→「Create API key」'],
 ['3','複製那串 AIza 開頭的金鑰，先貼到記事本暫存'],
], [14*mm, 154*mm]))
story += [sp(10), P('Ⓒ 建立 Telegram 機器人（約 3 分鐘，只需要一串 token）', 'h2'),
 P('這是娜娜在 Telegram 上的「身分」。這步只要拿到一串 token，不用查任何 ID。'), sp(4)]
story.append(tbl(['步驟','動作'], [
 ['1','安裝並註冊 Telegram（App Store / Google Play）'],
 ['2','搜尋 @BotFather（藍勾勾官方帳號）→ 按 Start'],
 ['3','傳 /newbot，照指示取名字、取帳號（須以 bot 結尾，例如 nana_home_bot）'],
 ['4','BotFather 回傳一串 token（像 1234567890:ABCdef...），複製暫存'],
], [14*mm, 154*mm]))
story += [sp(6), P('⚠️ 每台娜娜要用「自己的」bot，兩台共用同一個會互搶訊息、兩台都收不到。', 'small'), sp(8),
 P('Ⓓ 填進設定頁、儲存', 'h2')]
story.append(tbl(['欄位','填什麼'], [
 ['WiFi 名稱／密碼','你家的 WiFi（2.4GHz）'],
 ['Gemini API Key','Ⓑ 拿到的 AIza... 那串'],
 ['Telegram Bot Token','Ⓒ 拿到的 1234567890:ABC... 那串'],
], [46*mm, 122*mm]))
story += [sp(6), P('填完按「儲存並重啟裝置」，等約 30 秒——螢幕顯示「Telegram + Gemini ready」＝連網成功！', 'body'), sp(8),
 P('Ⓔ 點一下完成 Telegram 綁定（全程免查 ID）', 'h2'),
 P('裝置連上網路後，手機連**同一個家用 WiFi**，開它的控制台（網址見螢幕）：'), sp(4)]
story.append(tbl(['步驟','動作'], [
 ['1','用剛剛的 PIN 登入控制台'],
 ['2','點下方「設定」分頁，找到「🔗 Telegram 機器人」卡片'],
 ['3','按「點我加好友並綁定」'],
 ['4','手機自動開啟 Telegram、跳到你的機器人對話 → 按 Start'],
 ['5','裝置幾秒內回你「✅ 綁定成功！」＝完成！'],
], [14*mm, 154*mm]))
story += [sp(8), box([
 '<b>小提醒</b>',
 '．熱點只在「它連不上家裡 WiFi」時才出現。如果它已經連上網，本來就不會有熱點。',
 '．WiFi 密碼打錯了？別緊張——它連不上就會再開熱點，重來一次就好，它不會壞。',
 '．按「點我加好友並綁定」沒反應？裝置一定要先連上網路（完成 Ⓓ 才行），',
 '　剛存檔重啟的 30 秒內請稍等再按。',
 '．金鑰或 token 複製時多了空白？回設定頁重貼一次再存檔。']), PageBreak()]

# ── 第 3 章 ──
story += [P('第 3 章　跟它說話', 'h1'), hr(),
 P('方法一：喊它的名字（免持）', 'h2'),
 P('在 1～2 公尺內，用平常音量清楚地喊「<b>nana</b>」（娜娜）。它會回「我在，請說。」——'
   '接著把你要問的話說完，它會用喇叭回答你，螢幕同時顯示表情和文字摘要。'),
 P('※ 喚醒功能預設關閉。開啟方式：在 Telegram 傳「/audio wake on」，或直接跟它說「幫我打開喚醒詞」。', 'small'), sp(6),
 P('方法二：按按鈕對講（吵雜環境最穩）', 'h2'),
 P('按一下機身上的 BOOT 按鈕，螢幕顯示 Listening 後開始講話（約 5 秒）。'
   '它聽完會回答，並把你說的話和回覆同步傳到 Telegram 留紀錄。'), sp(6),
 P('方法三：打字（Telegram）', 'h2'),
 P('見第 4 章。三種方法找到的都是同一個娜娜，記憶互通。'), sp(8),
 P('讓它安靜／出聲：語音模式', 'h2')]
story.append(tbl(['模式','效果','怎麼切換'], [
 ['normal','完整唸出每一句回覆','說「用正常模式說話」或傳 /voice normal'],
 ['brief','只唸重點第一句','說「講重點就好」或傳 /voice brief'],
 ['off','完全靜音（螢幕照常顯示）','說「安靜」或傳 /voice off'],
], [24*mm, 68*mm, 76*mm]))
story += [sp(10), P('想讓它的聲音更好聽（選配）', 'h2'),
 P('預設用的是 Google 免費語音，聽得懂，但機械感比較重。想換成自然的女生聲音，'
   '在家裡任一台常開的電腦上做兩件事：'), sp(4)]
story.append(tbl(['步驟','動作'], [
 ['1','安裝：在命令列輸入 <b>pip install edge-tts</b>'],
 ['2','執行韌體資料夾裡的 <b>tools/tts_proxy.py</b>，它會印出一個網址'],
 ['3','控制台 → 設定 → 「好聽音色」→ 把網址貼進去 → 儲存'],
], [14*mm, 154*mm]))
story += [sp(6), P('那台電腦開著的時候就是自然女聲；電腦關了它會自動退回 Google 語音，'
   '不會突然啞掉。音色可以改成台灣女聲、情感更豐富的版本等，設定頁有寫。', 'small'), sp(10),
 P('可以問什麼？隨便舉幾個：', 'h2'),
 P('「幫我翻譯：明天的會議延到下週」／「337 乘 24 是多少」／「恐龍為什麼滅絕」／'
   '「幫我想三個晚餐菜色」／「用 Python 寫一個猜數字遊戲」／「我今天心情不太好」', 'body_i'), PageBreak()]

# ── 第 4 章 ──
story += [P('第 4 章　用 Telegram 跟它當網友', 'h1'), hr(),
 P('Telegram 是免費的通訊 App（類似 LINE）。第 2 章 Ⓔ 綁定完成後，'
   '娜娜已經是你的 Telegram 好友，你人在世界任何角落都能跟家裡的它傳訊息。'), sp(6),
 P('傳一句「你好」試試——它會回你。忘記機器人名稱？在裝置控制台「設定」分頁的'
   '「🔗 Telegram 機器人」卡片可以看到。'), sp(8),
 P('在 Telegram 能做的事', 'h2'),
 P('．所有聊天問答（和語音問的是同一個大腦、同一份記憶）<br/>'
   '．傳語音訊息給它——它會自動聽懂再回覆<br/>'
   '．傳照片給它——它會幫你分析照片內容<br/>'
   '．遠端拍照：傳「/camera snap」拍一張家裡的照片，或「拍張照告訴我桌上有什麼」<br/>'
   '．每天早上 8 點，它會主動傳「今日簡報」：天氣、新聞、今天的提醒'), sp(8),
 box(['<b>安全設計</b>：娜娜只認你一個人的 Telegram 帳號，其他人傳訊息給它一律已讀不回。']), PageBreak()]

# ── 第 5 章 ──
story += [P('第 5 章　它會記得你說的事', 'h1'), hr(),
 P('直接用講的（或打字）：'), sp(4),
 P('「<b>記住</b>我媽的生日是 3 月 12 日」→ 它存起來了<br/>'
   '「<b>記住</b>備份鑰匙放在紅色餅乾盒」→ 存起來了<br/>'
   '之後隨時問：「我媽生日是哪天？」——它會答對。', 'body_i'), sp(6),
 P('管理記憶：傳「/memories」看它記了哪些事；「忘記鑰匙那件事」或「/forget 鑰匙」可以刪除。'
   '最多可記 30 筆，滿了會自動汰舊。'), sp(4),
 P('※ 貼心提醒：密碼、卡號這類重要機密，不建議用講的存——把它當成「會轉述的朋友」就對了。', 'small'), sp(12),
 P('第 6 章　每日提醒', 'h1'), hr(),
 P('用白話設定就好：'), sp(4),
 P('「每天早上 8 點提醒我吃血壓藥」<br/>「每 30 分鐘提醒我喝水」<br/>「每天 21:30 提醒我倒垃圾」', 'body_i'), sp(6),
 P('時間一到，它會出聲唸提醒內容，同時發 Telegram 訊息（家人也看得到有提醒到）。'), sp(6)]
story.append(tbl(['指令','功能','範例'], [
 ['/reminders','列出所有提醒','/reminders'],
 ['/remind 8:00 訊息','每天固定時間提醒','/remind 8:00 吃藥'],
 ['/remind 30m 訊息','每 30 分鐘提醒','/remind 30m 喝水'],
 ['/remind del 編號','刪除某則提醒','/remind del 3'],
], [42*mm, 66*mm, 60*mm]))
story.append(PageBreak())

# ── 第 7 章 ──
story += [P('第 7 章　它的眼睛：相機', 'h1'), hr(),
 P('拍一張', 'h2'),
 P('Telegram 傳「/camera snap」，幾秒後照片就傳回你手機。'), sp(4),
 P('拍照＋請 AI 幫你看', 'h2'),
 P('「拍張照，桌上有沒有我的眼鏡？」——它會拍照、看圖、用文字告訴你答案。'
   '也可以傳「/camera vision 有人在家嗎」。'), sp(4),
 P('即時影像', 'h2'),
 P('打開網頁控制台（第 9 章）→ 主控分頁 → 按「開始直播」。畫面約每秒更新 1 格——'
   '這是設計上的正常速度，適合「看一眼確認狀況」，不是監視器畫質。看完記得按停止。'), sp(4),
 P('定時拍照（植物日記、看家）', 'h2'),
 P('「每 30 分鐘拍一張傳給我」——它就會乖乖執行，你的 Telegram 會定時收到照片。'
   '詳見第 8 章。'), sp(12),
 P('第 8 章　自動化排程', 'h1'), hr(),
 P('讓它定時「做事」。四種動作：'), sp(4)]
story.append(tbl(['動作','做什麼','範例（用講的就行）'], [
 ['有事才通知','定時檢查一件事，<b>只有真的發生才通知你</b>，沒發生完全安靜','「每 5 分鐘看有沒有人趴著睡覺，有才叫我」'],
 ['定時拍照','拍照＋AI 分析＋傳 Telegram（每次都報），可加語音播報','「每 10 分鐘拍照看貓在幹嘛」'],
 ['GPIO 供電','定時對腳位供電/斷電，接繼電器可控制燈、風扇等','「每 60 分鐘打開 21 號腳位」'],
 ['定時警報','大聲播提示音＋唸訊息＋傳 Telegram','「每 30 分鐘提醒大家休息，要大聲」'],
], [26*mm, 70*mm, 72*mm]))
story += [sp(8), box([
 '<b>「有事才通知」是最實用的一種</b>',
 '一般定時拍照每次都會傳一則訊息給你，看久了就懶得看了。「有事才通知」不一樣：'
 '它每次都會自己判斷條件成不成立，<b>不成立就完全不吭聲</b>，成立才通知你並且說明看到什麼。',
 '在網頁控制台建立這種任務時，還能先開直播、用手指<b>在畫面上拉一個框</b>，'
 '它就只盯那一塊，其他地方一律忽略——例如只看門口、只看那台機器的數字看板。',
 '你不用會寫什麼咒語，用平常講話的方式寫就好，它會自己整理成嚴謹的判斷指令。'])]
story += [sp(6),
 P('管理任務：傳「/tasks」看清單、「/tasks del 編號」刪除、「/tasks clear」全部清除。'
   '緊急情況傳「/pause」可一鍵停止所有自動任務與 AI。'), sp(4),
 P('※ GPIO 接電器屬進階玩法，需要基本電子常識——請先讀第 13 章的安全須知，或找送禮的人幫忙接線。', 'small'), PageBreak()]

# ── 第 9 章 ──
story += [P('第 9 章　網頁控制台', 'h1'), hr(),
 P('手機/電腦連到<b>跟娜娜同一個 WiFi</b>，瀏覽器輸入它的 IP 位址'
   '（不知道 IP？在 Telegram 問它「你的 IP 是多少」或傳 /status）。'
   '第一次會要求輸入 6 位 PIN 碼（跟開機設定用的同一組），登入後 30 天內免重登。'), sp(6)]
story.append(tbl(['分頁','功能'], [
 ['主控','即時影像直播／打字跟它說話（回覆進 Telegram）／喇叭測試音'],
 ['排程','圖形化建立定時任務（有事才通知/拍照/GPIO/警報）、畫面框選、查看與刪除任務、每日提醒'],
 ['記憶','查看它記住的所有事情；下方「技能」依分類列出它會的每一件事，點開看怎麼講才會觸發'],
 ['設定','WiFi（兩組）、API 金鑰、AI 模型、語音模式、好聽音色；儲存後自動重啟生效'],
], [24*mm, 144*mm]))
story += [sp(8), box([
 '<b>不用 Telegram 也能完整使用</b>',
 'WiFi 設定好之後，控制台本身就能叫它做事、建立任務、看它會什麼。'
 'Telegram 只是多一個「人在外面也收得到通知」的管道，不想用可以跳過。']), sp(6),
 P('看不懂該怎麼叫它做事？先去「記憶」分頁下方的<b>技能</b>看一眼——'
   '每個技能都寫了什麼講法會觸發它，照那個意思講就會通，不用背指令。', 'tip')]
story += [sp(8), box([
 '<b>安全設計</b>：控制台有 PIN 保護，連錯 5 次會鎖 5 分鐘；WiFi 密碼在頁面上永遠以 **** 顯示；'
 '只有跟它同一個 WiFi 的人才有機會開啟這個頁面。']), sp(6),
 P('小技巧：手機瀏覽器開啟後選「加入主畫面」，以後點圖示就像開 App 一樣方便。', 'tip'), PageBreak()]

# ── 第 10 章 ──
story += [P('第 10 章　生活應用點子（12 種玩法）', 'h1'), hr()]
ideas = [
 ['桌面陪伴','放書桌上，工作累了隨口聊兩句。它是「一直都在」的夥伴，不用解鎖、不用開 App'],
 ['長輩問安機','放長輩客廳：每天定時語音問候＋提醒吃藥；家人從 Telegram 收定時照片安心確認'],
 ['吃藥提醒','「每天早上 8 點提醒我吃藥」——喇叭主動出聲，比手機鬧鐘更難忽略'],
 ['寵物監視','對準寵物區，出門設定時拍照；發現拆家可遠端觸發警報聲嚇阻'],
 ['植物日記','「每 30 分鐘拍一張」，幾週後 Telegram 相簿就是一部開花縮時史'],
 ['出門看家','對準玄關定時拍照；覺得可疑遠端拍一張或開即時影像確認'],
 ['隨身助理','上班用 Telegram 問它：翻譯、算數、寫程式、查資料——家裡的它就是你的後台'],
 ['小孩問答機','小孩喊「nana」問問題——不用給手機平板，沒有影片遊戲分心'],
 ['廚房幫手','滿手麵粉時用喊的：「三大匙是幾 cc」「記住醬油要補貨」'],
 ['電器總管','GPIO 接繼電器：魚缸燈定時亮、聖誕燈定時開（進階，見安全須知）'],
 ['記憶祕書','鑰匙放哪、誰的生日、車位號碼——用講的存、用問的取'],
 ['旅行分身','出國用 Telegram 看家裡照片＋讓燈定時亮製造有人在家的感覺'],
]
story.append(tbl(['玩法','怎麼用'], ideas, [30*mm, 138*mm]))
story.append(PageBreak())

# ── 第 11 章 ──
story += [P('第 11 章　搬家、換 WiFi 怎麼辦？', 'h1'), hr(),
 P('一句話：<b>它連不上網就會自動變回熱點模式，重做第 2 章的 Ⓐ+Ⓓ 填新 WiFi 就好，'
   '不用重新申請金鑰、也不用重新綁定 Telegram。</b>'), sp(6),
 P('換了路由器、改了 WiFi 密碼、搬到新家——插上電等 1～2 分鐘，'
   '螢幕出現熱點名稱和 PIN，手機連上去重填新的 WiFi 就完成。金鑰、Telegram 綁定、'
   '記憶、提醒全都還在，不會不見。'), sp(4),
 P('停電也不怕：所有設定存在機器裡面，來電後自動開機恢復原狀。'), sp(12),
 P('第 12 章　常見問題 FAQ', 'h1'), hr()]
faq = [
 ['喊「nana」都不理我？','先確認喚醒功能有開（傳 /audio wake on）。離它 1～2 公尺、環境別太吵。'
  '連喊三次沒反應就按 BOOT 鈕對講，一定通。螢幕沒表情=可能沒連上網，見第 11 章'],
 ['找不到它的 WiFi 熱點？','熱點只在「它連不上家裡 WiFi」時出現。已連網就不會有熱點——直接用就好。剛插電請等 1～2 分鐘'],
 ['手機說這個 WiFi 沒有網路？','正常！這熱點本來就只拿來設定用，選「仍要連線／維持連線」繼續，不要斷開'],
 ['WiFi 密碼打錯了？','它連不上會自動再開熱點，重連重打一次就好。它不會壞，只會等你'],
 ['按「點我加好友並綁定」沒反應？','裝置要先連上網路（完成第 2 章 Ⓓ 存檔重啟）這顆按鈕才會動；'
  '剛重啟的 30 秒內請稍等再試'],
 ['Telegram 找不到機器人？','裝置控制台「設定」分頁的「🔗 Telegram 機器人」卡片可以看到機器人名稱，複製後到 Telegram 搜尋'],
 ['控制台網址是什麼？','剛設定完會顯示在裝置螢幕上；忘記的話可傳 Telegram 訊息「你的網址是多少」問它。'
  '注意：手機要和它連同一個 WiFi；在外面請改用 Telegram'],
 ['忘記 PIN 碼？','看送禮小卡上記錄的 PIN；真的都忘了，聯絡送你禮物的人協助'],
 ['即時影像卡卡的？','正常。設計是約每秒 1 格，適合看一眼確認狀況。要清楚畫面請用拍照'],
 ['回答要等幾秒？','正常。它把問題送到雲端 AI 思考再回來。等很久沒回應，先檢查家裡網路'],
 ['拔插頭設定會消失嗎？','不會。WiFi、記憶、提醒都存在機器裡，重新插電全部回來'],
 ['它是不是一直在錄音錄影上傳？','不是。麥克風平常只在機器本身聽有沒有人喊它，聽到之後才把那句話送出處理；'
  '相機只在你下指令或你自己設了定時拍照時才動作。詳見第 13 章'],
]
story.append(tbl(['問題','解答'], faq, [46*mm, 122*mm]))
story.append(PageBreak())

# ── 第 13 章 ──
story += [P('第 13 章　安全與隱私須知（請務必閱讀）', 'h1'), hr(),
 P('相機與隱私', 'h2'),
 P('．家裡有其他人時，先告訴他們這台機器有鏡頭。<br/>'
   '．不要對準床、更衣區、浴室方向；也不要正對鄰居的窗戶或門口（可能觸法）。<br/>'
   '．出門看家設的定時拍照，回家後記得關（不然會一直拍到自己）。'), sp(6),
 P('麥克風', 'h2'),
 P('．喚醒功能開啟時，它會持續在機器本身聆聽喚醒詞——這是語音功能的必要運作。'
   '介意時可以關閉喚醒（/audio wake off）或放到不常談話的房間。<br/>'
   '．密碼、卡號等機密不要用講的給它記。'), sp(6),
 P('用電安全', 'h2'),
 Paragraph('．使用 5V 規格的變壓器；機身微溫正常，但不要用布蓋住或塞進密閉櫃子。<br/>'
   '．<b>GPIO 只能接低壓小電流模組（LED、5V 繼電器的訊號端）。'
   '絕對不要把 110V 市電直接接上機器！</b>要控制家電必須透過合格繼電器模組，'
   '強烈建議由懂電的人協助；不確定就先不要接。', S['warn']), sp(6),
 P('放置位置', 'h2'),
 P('．平穩桌面，遠離水槽、瓦斯爐、飲料杯；避免陽光直射。<br/>'
   '．警報音量大，不要貼著嬰兒床或耳朵高度擺放。<br/>'
   '．線材沿牆整理，避免絆倒或被寵物咬。'), sp(6),
 P('帳號安全', 'h2'),
 P('．它只回應綁定的 Telegram 帳號；機器人名稱不要隨意公開貼上網路。<br/>'
   '．控制台 PIN 不要改成 0000、1234 這種好猜的數字。'), PageBreak()]

# ── 附錄 ──
story += [P('附錄　Telegram 指令速查表', 'h1'), hr(),
 P('大部分功能用白話講就可以，指令是給想精確控制的你。指令不分大小寫。', 'small'), sp(4)]
cmds = [
 ['/help','顯示完整指令選單'],
 ['/status','系統狀態總覽（連線、模型、記憶體、IP…）'],
 ['/briefing','立刻來一份今日簡報（天氣＋新聞＋提醒）'],
 ['/camera snap','拍一張照片傳給你'],
 ['/camera vision 問題','拍照＋AI 分析回答'],
 ['/camera stream','產生即時影像網址（同 WiFi 瀏覽器觀看）'],
 ['/memories','列出所有記憶'],
 ['/remember 主題 內容','手動存一筆記憶'],
 ['/forget 主題','刪除記憶'],
 ['/reminders','列出提醒；del 編號＝刪除；clear＝清空'],
 ['/remind 8:00 吃藥','每天 8 點提醒'],
 ['/tasks','列出自動任務；del 編號＝刪除；clear＝清空'],
 ['/autotask add camera 5 描述','每 5 分鐘拍照分析（加 speak 會用唸的）'],
 ['/sleepwatch on / off','睡眠監視模式開／關（每分鐘拍照播報）'],
 ['/voice off / brief / normal','靜音／講重點／完整語音'],
 ['/audio wake on / off','喚醒詞開／關'],
 ['/audio say 文字','讓它唸出指定文字'],
 ['/audio tone','播測試音（檢查喇叭）'],
 ['/model','查看／切換 AI 模型'],
 ['/wifi','WiFi 管理（set／set2／del／clear）'],
 ['/skills','查看 16 個內建技能'],
 ['/pause　/resume','緊急暫停全部功能／恢復'],
 ['/diag','自我診斷報告（回報問題時很有用）'],
]
story.append(tbl(['指令','功能'], cmds, [62*mm, 106*mm]))
story += [sp(10), hr(),
 P('硬體規格：ESP32-S3（16MB Flash／8MB PSRAM）·2.4GHz WiFi·藍牙·240×320 彩色螢幕·'
   '200 萬畫素相機·喇叭與麥克風·USB 5V 供電', 'small'), sp(4),
 P('AI 引擎：Google Gemini（雲端）·語音喚醒：本機離線偵測·韌體 SerialSoul v5.3', 'small'), sp(16),
 Paragraph('生日快樂！祝你和娜娜相處愉快', ParagraphStyle('end', parent=S['cover_sub'], fontSize=13))]

doc = SimpleDocTemplate(
    str(Path(__file__).resolve().parent.parent / 'docs' / '娜娜使用手冊_送禮版.pdf'),
    pagesize=A4, topMargin=20*mm, bottomMargin=22*mm, leftMargin=20*mm, rightMargin=20*mm,
    title='娜娜智慧陪伴機 使用手冊', author='SerialSoul')
doc.build(story, onFirstPage=lambda c,d: None, onLaterPages=footer)
print('OK: 娜娜使用手冊_送禮版.pdf')
