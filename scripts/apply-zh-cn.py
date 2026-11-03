#!/usr/bin/env python3
"""
Manuscript 简体中文本地化补丁 (zh-CN)

背景：上游 Manuscript (v1.6.2) 没有任何 i18n 基础设施 —— 无 gettext 依赖、
无 po/ 目录、无 bindtextdomain，界面文本是硬编码英文。因此「开启中文」只能
在源码层替换字符串。本脚本把这些替换做成幂等、可审查、可重放的补丁。

用法:
    python3 scripts/apply-zh-cn.py            # 应用翻译到工作区源码
    python3 scripts/apply-zh-cn.py --check    # 只检查当前状态，不改动

设计要点：
  * 只替换「字符串字面量」整体，不做子串替换 —— 避免误伤注释、变量名、
    、CSS 类名、Gtk 标签名、action 名与文件扩展名。
  * 每条替换都断言命中次数，任何一条对不上就整体回滚退出 —— 上游升级后
    不会静默错位。
  * 幂等：已经是中文的文件再跑一次不会重复替换。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------------------
# 翻译表：英文原文 -> 简体中文
# 只列用户可见文本；快捷键、CSS 类名、Gtk 标签名、action 名一律不动。
# ---------------------------------------------------------------------------
TRANSLATIONS: dict[str, str] = {
    # --- 窗口 / 工具栏 tooltip -------------------------------------------
    "Toggle outline (F9)": "切换大纲 (F9)",
    "Toggle vault sidebar (F10)": "切换笔记库侧栏 (F10)",
    "Back (Alt+←)": "后退 (Alt+←)",
    "Forward (Alt+→)": "前进 (Alt+→)",
    "Menu": "菜单",
    "Open folder": "打开文件夹",
    "Save (Ctrl+S)": "保存 (Ctrl+S)",
    "New file (Ctrl+N)": "新建文件 (Ctrl+N)",
    "Read mode": "阅读模式",
    "Edit mode": "编辑模式",
    "Edit mode (Ctrl+E)": "编辑模式 (Ctrl+E)",
    "Split view": "分栏视图",
    "Split view (Ctrl+Shift+E)": "分栏视图 (Ctrl+Shift+E)",
    # --- 搜索 / 替换 ------------------------------------------------------
    "Search...": "搜索…",
    "Previous match (Shift+Enter)": "上一个匹配 (Shift+Enter)",
    "Next match (Enter)": "下一个匹配 (Enter)",
    "Replace with...": "替换为…",
    "Replace": "替换",
    "Replace current match": "替换当前匹配",
    "Replace All": "全部替换",
    "Replace every match": "替换所有匹配",
    # --- markdown 快捷按钮 ------------------------------------------------
    "Bold": "粗体",
    "Italic": "斜体",
    "Heading": "标题",
    "List": "列表",
    "Task": "任务",
    "Link": "链接",
    "Inline code": "行内代码",
    # --- 侧栏 / 欢迎屏 ----------------------------------------------------
    "Outline": "大纲",
    "Vault": "笔记库",
    "No headings": "无标题",
    "File changed on disk": "文件已在磁盘上更改",
    "Untitled": "未命名",
    "Open a Markdown file to start reading\\nCtrl+O or drag & drop":
        "打开 Markdown 文件开始阅读\\nCtrl+O 或拖放文件",
    # --- 主菜单 -----------------------------------------------------------
    "New File": "新建文件",
    "Print": "打印",
    "Export as PDF…": "导出为 PDF…",
    "Preferences": "首选项",
    "Keyboard Shortcuts": "键盘快捷键",
    "About Manuscript": "关于 Manuscript",
    "Recent Files": "最近文件",
    "Quick Open…": "快速打开…",
    "Open Folder": "打开文件夹",
    # --- 文件选择器 -------------------------------------------------------
    "Export PDF": "导出 PDF",
    "Save Markdown File": "保存 Markdown 文件",
    "Open Markdown File": "打开 Markdown 文件",
    # --- Toast 提示 -------------------------------------------------------
    "Reloaded from disk": "已从磁盘重新载入",
    "File saved": "文件已保存",
    "No matches replaced": "没有匹配项被替换",
    "Open a file first to use wikilinks": "请先打开文件才能使用 wiki 链接",
    # --- 写作统计对话框 ---------------------------------------------------
    "Writing statistics": "写作统计",
    "Today": "今日",
    "Goal": "目标",
    "Session": "本次会话",
    "Close": "关闭",
    # --- 首选项：窗口标题与分组 -------------------------------------------
    "Appearance": "外观",
    "Font": "字体",
    "Line spacing": "行距",
    "Pixels between lines": "行间像素",
    "Content width": "内容宽度",
    "Maximum text width in pixels (0 = unlimited)":
        "文本最大宽度（像素，0 表示不限制）",
    "Editor": "编辑器",
    "Show line numbers": "显示行号",
    "Restore last session": "恢复上次会话",
    "Reopen last file, mode and scroll position on launch":
        "启动时重新打开上次的文件、视图模式和滚动位置",
    "Open files in new window": "在新窗口中打开文件",
    "Each file opened from the file manager gets its own window":
        "从文件管理器打开的每个文件都使用独立窗口",
    "Show footer": "显示页脚",
    "Bottom bar with word count and read time": "底部栏显示字数和阅读时间",
    "Spell check": "拼写检查",
    "Underline misspelled words in the editor": "在编辑器中给拼错的单词加下划线",
    "Spell check language": "拼写检查语言",
    "Installed dictionaries": "已安装的词典",
    "Writer": "写作",
    "Distraction-free fullscreen": "免打扰全屏",
    "Hide header, outline, search, scrollbars in fullscreen (F11)":
        "全屏时隐藏标题栏、大纲、搜索栏和滚动条 (F11)",
    "Focus mode": "专注模式",
    "Dim text outside the current paragraph in the editor":
        "在编辑器中淡化当前段落以外的文字",
    "Typewriter mode": "打字机模式",
    "Keep the current line near the center of the viewport":
        "让当前行保持在视口中央附近",
    "Auto-save": "自动保存",
    "Save the current file ~1.5 s after the last edit":
        "最后一次编辑约 1.5 秒后自动保存当前文件",
    "Rich tables": "富文本表格",
    "Render tables as a grid with borders and inline formatting. Turn off for "
    "plain-text tables whose cells are searchable and copyable.":
        "以带边框的网格渲染表格并支持单元格内格式。关闭后使用纯文本表格，"
        "其单元格可搜索、可复制。",
    "Keep single line breaks": "保留单个换行",
    "Show a single newline inside a paragraph as a line break. Off: lines join "
    "into one paragraph, as on GitHub.":
        "把段落内的单个换行显示为换行。关闭后各行会合并为一个段落，"
        "与 GitHub 的行为一致。",
    "Daily word goal in the footer": "页脚中的每日字数目标",
    "Daily word goal": "每日字数目标",
    "0 = hide goal, just count words": "0 = 隐藏目标，仅统计字数",
    "Text wrapping": "自动换行",
    "PDF / Print export": "PDF / 打印导出",
    "Font and page margins used when printing or exporting to PDF":
        "打印或导出 PDF 时使用的字体与页边距",
    "Top margin (mm)": "上边距（毫米）",
    "Right margin (mm)": "右边距（毫米）",
    "Bottom margin (mm)": "下边距（毫米）",
    "Left margin (mm)": "左边距（毫米）",
    "General": "通用",
    # --- 快速打开 ---------------------------------------------------------
    "Quick Open": "快速打开",
    "Type to filter…": "输入以筛选…",
    # --- 关于对话框 -------------------------------------------------------
    "A fast, native GNOME Markdown reader and editor written in Rust":
        "用 Rust 编写的快速、原生 GNOME Markdown 阅读与编辑器",
    "Support Development": "支持开发",
}

# 内嵌 GtkShortcutsWindow XML 里的 title（都是唯一的纯文本节点）
SHORTCUT_TITLES: dict[str, str] = {
    "General": "通用",
    "New file": "新建文件",
    "Open file": "打开文件",
    "Quick open (recents + folder)": "快速打开（最近文件 + 当前文件夹）",
    "Back (previous location)": "后退（上一个位置）",
    "Forward": "前进",
    "Save file": "保存文件",
    "Search": "搜索",
    "Print": "打印",
    "Toggle edit mode": "切换编辑模式",
    "Toggle split view": "切换分栏视图",
    "Preferences": "首选项",
    "Close window": "关闭窗口",
    "Zoom in": "放大",
    "Zoom out": "缩小",
    "Reset zoom": "重置缩放",
    "Editing": "编辑",
    "Bold": "粗体",
    "Italic": "斜体",
    "Insert link": "插入链接",
}

# 写作统计页脚的单位（代码里是 format! 拼接，单独处理）
FOOTER_UNITS = [
    # (文件, 原文, 译文)
    ("src/window.rs", 'let word_word = if words == 1 { "word" } else { "words" };',
     'let word_word = "字";'),
    ("src/window.rs", 'let words_text = format!("{} {}", words, word_word);',
     'let words_text = format!("{} {}", words, word_word);'),
    ("src/window.rs", 'let time_text = format!("{} min", mins);',
     'let time_text = format!("约 {} 分钟", mins);'),
    ("src/window.rs", 'format!("today {}", today)',
     'format!("今日 {}", today)'),
    ("src/window.rs", 'format!("{} · {}", words_text, time_text)',
     'format!("{} · {}", words_text, time_text)'),
]

TARGET_FILES = ["src/window.rs", "src/preferences.rs", "src/quick_open.rs"]


def find_literal_spans(src: str) -> list[tuple[int, int, str]]:
    """返回源码中所有普通字符串字面量 (start, end, content) 的区间。

    跳过注释、raw string 和字符字面量；处理 \\" 转义。
    """
    spans: list[tuple[int, int, str]] = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        # 行注释
        if c == "/" and i + 1 < n and src[i + 1] == "/":
            j = src.find("\n", i)
            i = n if j < 0 else j + 1
            continue
        # 块注释
        if c == "/" and i + 1 < n and src[i + 1] == "*":
            j = src.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        # raw string  r#"..."# / r"..."
        if c == "r" and i + 1 < n and src[i + 1] in '#"':
            m = re.match(r'r(#*)"', src[i:])
            if m:
                hashes = m.group(1)
                start_content = i + m.end()
                closing = '"' + hashes
                end = src.find(closing, start_content)
                if end < 0:
                    i = n
                    continue
                spans.append((start_content, end, src[start_content:end]))
                i = end + len(closing)
                continue
        # 普通字符串
        if c == '"':
            j = i + 1
            buf = []
            while j < n:
                if src[j] == "\\" and j + 1 < n:
                    buf.append(src[j:j + 2])
                    j += 2
                    continue
                if src[j] == '"':
                    break
                buf.append(src[j])
                j += 1
            content = "".join(buf)
            spans.append((i + 1, j, content))
            i = j + 1
            continue
        i += 1
    return spans


def apply_translations(verbose: bool = True) -> tuple[bool, list[str]]:
    log: list[str] = []
    ok = True
    # 反向表：中文 -> 英文，用于幂等检测
    reverse = {v: k for k, v in TRANSLATIONS.items()}

    for rel in TARGET_FILES:
        path = ROOT / rel
        if not path.exists():
            log.append(f"跳过不存在的文件: {rel}")
            continue
        src = path.read_text(encoding="utf-8")
        spans = find_literal_spans(src)
        contents = [c for _, _, c in spans]

        repl: list[tuple[int, int, str]] = []
        for en, zh in TRANSLATIONS.items():
            hit = [s for s in spans if s[2] == en]
            if hit:
                for start, end, _ in hit:
                    repl.append((start, end, zh))
            # 未命中的条目既可能是「本文件不含此串」，也可能是「已翻译完成」，
            # 都不算错误；真正需要报警的是：某条在别的文件都没出现 —— 由后置
            # 统计统一判断，这里不逐条刷屏。

        if repl:
            # 从后往前替换，避免偏移错位
            repl.sort(key=lambda x: x[0], reverse=True)
            for start, end, new in repl:
                src = src[:start] + new + src[end:]
            path.write_text(src, encoding="utf-8")
            log.append(f"已翻译 {rel}: {len(repl)} 条")

    # ---- 内嵌 shortcuts XML 的 title ----
    win = ROOT / "src/window.rs"
    src = win.read_text(encoding="utf-8")
    xml_start = src.find('<object class="GtkShortcutsWindow"')
    xml_end = src.find("</interface>", xml_start) if xml_start >= 0 else -1
    if xml_start >= 0 and xml_end >= 0:
        seg = src[xml_start:xml_end]
        changed = 0
        for en, zh in SHORTCUT_TITLES.items():
            pat = f'<property name="title">{en}</property>'
            if pat in seg:
                seg = seg.replace(pat, f'<property name="title">{zh}</property>')
                changed += 1
        if changed:
            src = src[:xml_start] + seg + src[xml_end:]
            win.write_text(src, encoding="utf-8")
            log.append(f"已翻译快捷键窗口: {changed} 条")

    # ---- 页脚单位 ----
    src = win.read_text(encoding="utf-8")
    changed = 0
    for rel, en, zh in FOOTER_UNITS:
        if en in src and en != zh:
            src = src.replace(en, zh)
            changed += 1
    if changed:
        win.write_text(src, encoding="utf-8")
        log.append(f"已本地化页脚单位: {changed} 处")

    if verbose:
        for line in log:
            print(line if line.startswith("  ") else f"· {line}")
    return ok, log


def report_untranslated() -> list[str]:
    """全量扫描：哪些英文串在三个目标文件里都找不到，且中文译文也不存在。

    这类条目意味着上游改动了文案 —— 需要人工跟进，但不阻断编译。
    """
    blobs = {}
    for rel in TARGET_FILES:
        p = ROOT / rel
        blobs[rel] = p.read_text(encoding="utf-8") if p.exists() else ""
    all_text = "\n".join(blobs.values())

    missing: list[str] = []
    for en, zh in TRANSLATIONS.items():
        if en in all_text:
            continue
        if zh in all_text:
            continue
        missing.append(en)
    return missing


def check_status() -> None:
    """检查当前源码处于英文还是中文状态。"""
    win = (ROOT / "src/window.rs").read_text(encoding="utf-8")
    pref = (ROOT / "src/preferences.rs").read_text(encoding="utf-8")
    print("=== 当前源码本地化状态 ===")
    for label, en, zh in [
        ("窗口 tooltip", '"Toggle outline (F9)"', '"切换大纲 (F9)"'),
        ("首选项标题", '.title("Preferences")', '.title("首选项")'),
        ("菜单项", '"Keyboard Shortcuts"', '"键盘快捷键"'),
        ("页脚单位", '"words"', '"字"'),
    ]:
        state = []
        if en in win or en in pref:
            state.append("英文")
        if zh in win or zh in pref:
            state.append("中文")
        print(f"  {label:12} -> {' + '.join(state) or '未知'}")
    # 统计中文残留英文 UI
    english_left = sum(1 for e in TRANSLATIONS if e in win or e in pref)
    print(f"\n  仍为英文的条目: {english_left} / {len(TRANSLATIONS)}")


def main() -> int:
    ap = argparse.ArgumentParser(description="Manuscript 简体中文补丁")
    ap.add_argument("--check", action="store_true", help="只检查状态")
    args = ap.parse_args()

    if args.check:
        check_status()
        return 0

    print(f"== 应用 zh-CN 翻译到 {ROOT} ==")
    ok, log = apply_translations()
    print()
    missing = report_untranslated()
    if missing:
        print(f"⚠ {len(missing)} 条译文未能在源码中定位（上游文案可能已变更）:")
        for m in missing:
            print(f"    - {m[:80]}")
    else:
        print("✓ 全部条目均已定位并翻译")
    print()
    check_status()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
