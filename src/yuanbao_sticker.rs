//! Yuanbao sticker (TIMFaceElem) support — port of hermes
//! `gateway/platforms/yuanbao_sticker.py` @ v2026.8.3.
//!
//! The builtin 59-sticker catalog (hermes `STICKER_MAP`, originally
//! `builtin-stickers.json`) plus the fuzzy lookup pipeline (exact
//! name → name containment → description containment → multiset /
//! bigram / subsequence scoring) and the TIMFaceElem builders.
//!
//! Wire format (hermes): TIMFaceElem carries `index` (always 0 for
//! catalog stickers, so it is omitted on the wire per proto3
//! zero-default) and a `data` JSON string with `sticker_id` /
//! `package_id` / `width` / `height` / `formats` / `name`.
//!
//! ulnclaw surface (adaptation): hermes triggers stickers through the
//! `send_sticker` tool; ulnclaw exposes them as a `STICKER:<name>`
//! reply tag (mirroring the `MEDIA:` convention) and renders inbound
//! TIMFaceElem as `[emoji: <name>]` — name read from the data JSON
//! with a catalog reverse-lookup by `sticker_id` as fallback (hermes
//! reads the name field only).
//!
//! Adaptation: hermes normalises queries with full NFKC; ulnclaw folds
//! fullwidth ASCII (U+FF01..U+FF5E) and the ideographic space instead,
//! which covers the practical cases without a normalisation crate.

use crate::yuanbao_proto as proto;

/// One builtin sticker (hermes `STICKER_MAP` entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sticker {
    pub sticker_id: &'static str,
    pub package_id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub width: u32,
    pub height: u32,
    pub formats: &'static str,
}

/// hermes `STICKER_MAP` — builtin catalog in original insertion order.
pub static STICKER_CATALOG: &[Sticker] = &[
    Sticker {
        sticker_id: "278",
        package_id: "1003",
        name: "六六六",
        description: "666 厉害 牛 棒 绝了 好强 awesome",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "262",
        package_id: "1003",
        name: "我想开了",
        description: "想开 佛系 释怀 顿悟 看淡了 无所谓",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "130",
        package_id: "1003",
        name: "害羞",
        description: "腼腆 不好意思 脸红 娇羞 羞涩 捂脸",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "252",
        package_id: "1003",
        name: "比心",
        description: "笔芯 爱你 爱心手势 love heart 喜欢你",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "125",
        package_id: "1003",
        name: "委屈",
        description: "难过 想哭 可怜巴巴 瘪嘴 受伤 被欺负",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "146",
        package_id: "1003",
        name: "亲亲",
        description: "么么 mua 亲一下 kiss 飞吻 啵",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "131",
        package_id: "1003",
        name: "酷",
        description: "帅 墨镜 cool 高冷 有型 swagger",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "145",
        package_id: "1003",
        name: "睡",
        description: "睡觉 困 zzZ 打盹 躺平 休眠 sleepy",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "152",
        package_id: "1003",
        name: "发呆",
        description: "懵 愣住 放空 呆滞 出神 脑子空白",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "157",
        package_id: "1003",
        name: "可怜",
        description: "卖萌 求饶 委屈巴巴 弱小 拜托 眼巴巴",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "200",
        package_id: "1003",
        name: "摊手",
        description: "无奈 没办法 耸肩 随便 那咋整 whatever",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "213",
        package_id: "1003",
        name: "头大",
        description: "头疼 烦恼 郁闷 难搞 崩溃 一团乱",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "256",
        package_id: "1003",
        name: "吓",
        description: "害怕 惊恐 震惊 吓一跳 恐怖 怂",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "203",
        package_id: "1003",
        name: "吐血",
        description: "无语 崩溃 被雷 内伤 一口老血 屮",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "185",
        package_id: "1003",
        name: "哼",
        description: "傲娇 生气 不满 撇嘴 不理 赌气",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "220",
        package_id: "1003",
        name: "嘿嘿",
        description: "坏笑 猥琐笑 偷笑 憨笑 得意 你懂的",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "218",
        package_id: "1003",
        name: "头秃",
        description: "程序员 加班 焦虑 没头发 秃了 肝爆",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "221",
        package_id: "1003",
        name: "暗中观察",
        description: "窥屏 潜水 偷偷看 角落 围观 屏住呼吸",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "224",
        package_id: "1003",
        name: "我酸了",
        description: "嫉妒 柠檬精 羡慕 吃柠檬 眼红 恰柠檬",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "246",
        package_id: "1003",
        name: "打call",
        description: "应援 加油 支持 喝彩 助威 call",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "251",
        package_id: "1003",
        name: "庆祝",
        description: "祝贺 开心 耶 party 胜利 干杯",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "151",
        package_id: "1003",
        name: "奋斗",
        description: "努力 加油 拼搏 冲 干劲 卷起来",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "143",
        package_id: "1003",
        name: "惊讶",
        description: "震惊 哇 不敢相信 OMG 居然 这么离谱",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "144",
        package_id: "1003",
        name: "疑问",
        description: "问号 不懂 啥 为什么 啥情况 懵逼问",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "248",
        package_id: "1003",
        name: "仔细分析",
        description: "思考 推敲 认真 研究 琢磨 让我想想",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "184",
        package_id: "1003",
        name: "撅嘴",
        description: "嘟嘴 卖萌 不高兴 撒娇 嘴翘",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "199",
        package_id: "1003",
        name: "泪奔",
        description: "大哭 伤心 破防 感动哭 泪流满面 呜呜",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "276",
        package_id: "1003",
        name: "尊嘟假嘟",
        description: "真的假的 真假 可爱问 你骗我 是不是",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "113",
        package_id: "1003",
        name: "略略略",
        description: "调皮 吐舌 不服 略 气死你 鬼脸",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "180",
        package_id: "1003",
        name: "困",
        description: "想睡 倦 打哈欠 睁不开眼 好困啊 sleepy",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "181",
        package_id: "1003",
        name: "折磨",
        description: "难受 痛苦 煎熬 蚌埠住了 受不了 要命",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "182",
        package_id: "1003",
        name: "抠鼻",
        description: "不屑 无聊 淡定 无所谓 鄙视 挖鼻",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "183",
        package_id: "1003",
        name: "鼓掌",
        description: "拍手 叫好 赞同 666 喝彩 掌声",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "204",
        package_id: "1003",
        name: "斜眼笑",
        description: "滑稽 坏笑 doge 意味深长 阴阳怪气 嘿嘿嘿",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "216",
        package_id: "1003",
        name: "辣眼睛",
        description: "看不下去 cringe 毁三观 太丑了 瞎了",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "217",
        package_id: "1003",
        name: "哦哟",
        description: "惊讶 起哄 哇哦 有戏 不简单 哟",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "222",
        package_id: "1003",
        name: "吃瓜",
        description: "围观 看戏 八卦 路人 看热闹 板凳",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "225",
        package_id: "1003",
        name: "狗头",
        description: "doge 保命 开玩笑 滑稽 反讽 懂的都懂",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "227",
        package_id: "1003",
        name: "敬礼",
        description: "salute 尊重 收到 遵命 致敬 报告",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "231",
        package_id: "1003",
        name: "哦",
        description: "知道了 明白 敷衍 嗯 这样啊 收到",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "236",
        package_id: "1003",
        name: "拿到红包",
        description: "红包 谢谢老板 发财 开心 抢到了 欧气",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "239",
        package_id: "1003",
        name: "牛吖",
        description: "牛 厉害 强 666 佩服 大佬",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "272",
        package_id: "1003",
        name: "贴贴",
        description: "抱抱 亲昵 蹭蹭 亲密 靠靠 撒娇贴",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "138",
        package_id: "1003",
        name: "爱心",
        description: "心 love 喜欢你 红心 示爱 么么哒",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "170",
        package_id: "1003",
        name: "晚安",
        description: "好梦 睡了 night 早点休息 安啦 moon",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "176",
        package_id: "1003",
        name: "太阳",
        description: "晴天 早上好 阳光 morning 好天气 日",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "266",
        package_id: "1003",
        name: "柠檬",
        description: "酸 嫉妒 柠檬精 羡慕 我酸 恰柠檬",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "267",
        package_id: "1003",
        name: "大冤种",
        description: "倒霉 吃亏 自嘲 好心没好报 背锅 工具人",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "132",
        package_id: "1003",
        name: "吐了",
        description: "恶心 yue 受不了 嫌弃 想吐 生理不适",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "134",
        package_id: "1003",
        name: "怒",
        description: "生气 愤怒 火大 暴躁 气炸 怼",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "165",
        package_id: "1003",
        name: "玫瑰",
        description: "花 示爱 表白 浪漫 送你花 情人节",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "119",
        package_id: "1003",
        name: "凋谢",
        description: "花谢 失恋 难过 枯萎 心碎 凉了",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "159",
        package_id: "1003",
        name: "点赞",
        description: "赞 认同 好棒 good like 大拇指 顶",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "164",
        package_id: "1003",
        name: "握手",
        description: "合作 你好 商务 hello deal 成交 友好",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "163",
        package_id: "1003",
        name: "抱拳",
        description: "谢谢 失敬 江湖 承让 拜托 有礼",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "169",
        package_id: "1003",
        name: "ok",
        description: "好的 收到 没问题 okay 行 可以 懂了",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "174",
        package_id: "1003",
        name: "拳头",
        description: "加油 干 冲 fight 力量 击拳 硬气",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "191",
        package_id: "1003",
        name: "鞭炮",
        description: "过年 喜庆 爆竹 春节 噼里啪啦 红",
        width: 128,
        height: 128,
        formats: "png",
    },
    Sticker {
        sticker_id: "258",
        package_id: "1003",
        name: "烟花",
        description: "庆典 漂亮 新年 嘭 绽放 节日快乐",
        width: 128,
        height: 128,
        formats: "png",
    },
];

/// hermes `get_sticker_by_name` — fuzzy lookup by priority: exact
/// name, name containment (both directions), description containment,
/// then the best fuzzy search hit.
pub fn get_sticker_by_name(name: &str) -> Option<&'static Sticker> {
    let query = name.trim();
    if query.is_empty() {
        return None;
    }
    if let Some(sticker) = STICKER_CATALOG.iter().find(|s| s.name == query) {
        return Some(sticker);
    }
    if let Some(sticker) = STICKER_CATALOG
        .iter()
        .find(|s| s.name.contains(query) || query.contains(s.name))
    {
        return Some(sticker);
    }
    if let Some(sticker) = STICKER_CATALOG
        .iter()
        .find(|s| s.description.contains(query))
    {
        return Some(sticker);
    }
    search_stickers(query, 1).into_iter().next()
}

/// hermes `get_sticker_by_id` — exact `sticker_id` lookup.
pub fn get_sticker_by_id(sticker_id: &str) -> Option<&'static Sticker> {
    let sid = sticker_id.trim();
    if sid.is_empty() {
        return None;
    }
    STICKER_CATALOG.iter().find(|s| s.sticker_id == sid)
}

/// hermes `get_random_sticker` — random catalog entry, optionally
/// filtered to entries whose name/description contains `category`.
pub fn get_random_sticker(category: Option<&str>) -> &'static Sticker {
    if let Some(category) = category.filter(|c| !c.trim().is_empty()) {
        let candidates: Vec<&'static Sticker> = STICKER_CATALOG
            .iter()
            .filter(|s| s.description.contains(category) || s.name.contains(category))
            .collect();
        if !candidates.is_empty() {
            return candidates[random_index(candidates.len())];
        }
    }
    &STICKER_CATALOG[random_index(STICKER_CATALOG.len())]
}

fn random_index(bound: usize) -> usize {
    let mut bytes = [0u8; 4];
    crate::feishu::fill_random_bytes(&mut bytes);
    u32::from_le_bytes(bytes) as usize % bound
}

// ---------------------------------------------------------------------------
// Fuzzy search (hermes `_score_field` / `search_stickers`, aligned with
// chatbot-web yuanbao-openclaw-plugin/sticker-cache.ts searchStickers)
// ---------------------------------------------------------------------------

/// hermes `_normalize_text` — NFKC approximation (see module docs) +
/// trim + lowercase.
fn normalize_text(raw: &str) -> String {
    let folded: String = raw
        .chars()
        .map(|ch| {
            if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
                char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch)
            } else if ch == '\u{3000}' {
                ' '
            } else {
                ch
            }
        })
        .collect();
    folded.trim().to_lowercase()
}

/// hermes `_compact_text` — normalise, then strip the punctuation set.
fn compact_text(raw: &str) -> String {
    normalize_text(raw)
        .chars()
        .filter(|ch| !is_stripped_punct(*ch))
        .collect()
}

/// hermes `_PUNCT_RE` character class.
fn is_stripped_punct(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '-' | '_'
                | '·'
                | '.'
                | ','
                | '，'
                | '。'
                | '!'
                | '！'
                | '?'
                | '？'
                | '"'
                | '\u{201C}'
                | '\u{201D}'
                | '\''
                | '\u{2018}'
                | '\u{2019}'
                | '、'
                | '/'
                | '\\'
        )
}

/// hermes `_multiset_char_hit_ratio`.
fn multiset_char_hit_ratio(needle: &str, haystack: &str) -> f64 {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() {
        return 0.0;
    }
    let mut bag: std::collections::HashMap<char, usize> = std::collections::HashMap::new();
    for ch in haystack.chars() {
        *bag.entry(ch).or_insert(0) += 1;
    }
    let mut hits = 0usize;
    for ch in &needle_chars {
        if let Some(count) = bag.get_mut(ch) {
            if *count > 0 {
                hits += 1;
                *count -= 1;
            }
        }
    }
    hits as f64 / needle_chars.len() as f64
}

/// hermes `_bigram_jaccard`.
fn bigram_jaccard(a: &str, b: &str) -> f64 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.len() < 2 || b_chars.len() < 2 {
        return 0.0;
    }
    let a_set: std::collections::HashSet<(char, char)> =
        a_chars.windows(2).map(|w| (w[0], w[1])).collect();
    let b_set: std::collections::HashSet<(char, char)> =
        b_chars.windows(2).map(|w| (w[0], w[1])).collect();
    let inter = a_set.intersection(&b_set).count();
    let union = a_set.len() + b_set.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// hermes `_longest_subsequence_ratio` (greedy subsequence walk).
fn longest_subsequence_ratio(needle: &str, haystack: &str) -> f64 {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() {
        return 0.0;
    }
    let mut matched = 0usize;
    for ch in haystack.chars() {
        if matched >= needle_chars.len() {
            break;
        }
        if ch == needle_chars[matched] {
            matched += 1;
        }
    }
    matched as f64 / needle_chars.len() as f64
}

/// hermes `_score_field`.
fn score_field(haystack: &str, query: &str) -> f64 {
    let hay = normalize_text(haystack);
    let q = normalize_text(query);
    if hay.is_empty() || q.is_empty() {
        return 0.0;
    }
    let hay_c = compact_text(haystack);
    let q_c = compact_text(query);
    let q_len = q.chars().count();
    let mut best = 0.0f64;
    if hay == q {
        best = best.max(100.0);
    }
    if hay.contains(q.as_str()) {
        best = best.max(92.0 + q_len.min(6) as f64);
    }
    if q_len >= 2 && hay.starts_with(q.as_str()) {
        best = best.max(88.0);
    }
    if !q_c.is_empty() && hay_c.contains(q_c.as_str()) {
        best = best.max(86.0);
    }
    best = best.max(multiset_char_hit_ratio(&q_c, &hay_c) * 62.0);
    best = best.max(bigram_jaccard(&q_c, &hay_c) * 58.0);
    best = best.max(longest_subsequence_ratio(&q_c, &hay_c) * 52.0);
    if q_len == 1 && hay.contains(q.as_str()) {
        best = best.max(68.0);
    }
    best
}

/// hermes `search_stickers` — fuzzy-ranked catalog search.
///
/// Scoring combines substring, multiset char coverage, bigram Jaccard
/// and subsequence ratio over name/description; name outscores
/// description (×0.88). Empty queries return the catalog head.
pub fn search_stickers(query: &str, limit: usize) -> Vec<&'static Sticker> {
    let safe_limit = if limit == 0 { 10 } else { limit.clamp(1, 500) };
    let q_norm = normalize_text(query);
    if q_norm.is_empty() {
        return STICKER_CATALOG.iter().take(safe_limit).collect();
    }
    let mut scored: Vec<(f64, &'static Sticker)> = STICKER_CATALOG
        .iter()
        .map(|sticker| {
            let name_score = score_field(sticker.name, query);
            let desc_score = score_field(sticker.description, query) * 0.88;
            let mut id_score = 0.0f64;
            let sid_norm = normalize_text(sticker.sticker_id);
            if !sid_norm.is_empty() {
                if sid_norm == q_norm {
                    id_score = 100.0;
                } else if sid_norm.contains(q_norm.as_str()) {
                    id_score = 84.0;
                }
            }
            (name_score.max(desc_score).max(id_score), sticker)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top = scored.first().map(|(score, _)| *score).unwrap_or(0.0);
    if top <= 0.0 {
        return scored
            .into_iter()
            .take(safe_limit)
            .map(|(_, s)| s)
            .collect();
    }
    let floor = if top >= 22.0 {
        18.0
    } else if top >= 12.0 {
        10.0f64.max(top * 0.5)
    } else {
        6.0f64.max(top * 0.35)
    };
    let filtered: Vec<(f64, &'static Sticker)> = scored
        .iter()
        .filter(|(score, _)| *score >= floor)
        .copied()
        .collect();
    let source = if filtered.is_empty() {
        scored
    } else {
        filtered
    };
    source
        .into_iter()
        .take(safe_limit)
        .map(|(_, s)| s)
        .collect()
}

// ---------------------------------------------------------------------------
// TIMFaceElem builders (hermes `build_face_msg_body` /
// `build_sticker_msg_body`)
// ---------------------------------------------------------------------------

/// hermes `build_face_msg_body` — one TIMFaceElem. Yuanbao convention:
/// `index` is fixed at 0 for catalog stickers (server identifies the
/// sticker via the `data` JSON); a non-zero `face_index` is treated as
/// a legacy QQ face id.
pub fn build_face_msg_body(face_index: u64, data: Option<&str>) -> proto::MsgBodyElement {
    proto::MsgBodyElement {
        msg_type: "TIMFaceElem".into(),
        msg_content: proto::MsgContent {
            index: face_index,
            data: data.unwrap_or_default().to_string(),
            ..Default::default()
        },
    }
}

/// hermes `build_sticker_msg_body` — TIMFaceElem from a catalog entry
/// (`data` JSON with the original JS plugin field set and order).
pub fn build_sticker_msg_body(sticker: &Sticker) -> proto::MsgBodyElement {
    let data_payload = format!(
        "{{\"sticker_id\":{},\"package_id\":{},\"width\":{},\"height\":{},\"formats\":{},\"name\":{}}}",
        json_string(sticker.sticker_id),
        json_string(sticker.package_id),
        sticker.width,
        sticker.height,
        json_string(sticker.formats),
        json_string(sticker.name),
    );
    build_face_msg_body(0, Some(&data_payload))
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

// ---------------------------------------------------------------------------
// ulnclaw surface — `STICKER:<name>` reply tags + inbound rendering
// ---------------------------------------------------------------------------

/// Pull `STICKER:<name>` reply tags out of an outbound reply, mirroring
/// `crate::messaging::extract_media_tags`: lines whose name resolves in
/// the catalog are extracted (sent as TIMFaceElem), everything else is
/// left in the text.
pub fn extract_sticker_tags(text: &str) -> (String, Vec<String>) {
    let mut cleaned: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("STICKER:") {
            let candidate = rest.trim().trim_matches('`');
            if !candidate.is_empty() && get_sticker_by_name(candidate).is_some() {
                names.push(candidate.to_string());
                continue;
            }
        }
        cleaned.push(line.to_string());
    }
    let mut out = String::new();
    let mut prev_blank = false;
    for line in &cleaned {
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        prev_blank = blank;
    }
    (out.trim_end_matches('\n').to_string(), names)
}

/// Render an inbound TIMFaceElem (hermes: `[emoji: {name}]` /
/// `[emoji]`). Name comes from the `data` JSON, falling back to a
/// catalog reverse-lookup by `sticker_id`.
pub fn render_face_element(content: &proto::MsgContent) -> String {
    let mut face_name = String::new();
    if !content.data.is_empty() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content.data) {
            if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                face_name = name.trim().to_string();
            }
            if face_name.is_empty() {
                if let Some(sid) = value.get("sticker_id").and_then(|v| v.as_str()) {
                    if let Some(sticker) = get_sticker_by_id(sid) {
                        face_name = sticker.name.to_string();
                    }
                }
            }
        }
    }
    if face_name.is_empty() {
        "[emoji]".to_string()
    } else {
        format!("[emoji: {face_name}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_matches_hermes_shape() {
        assert_eq!(STICKER_CATALOG.len(), 59);
        let mut ids: Vec<&str> = STICKER_CATALOG.iter().map(|s| s.sticker_id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "sticker_id must be unique");
        assert!(STICKER_CATALOG.iter().all(|s| s.package_id == "1003"));
        assert!(STICKER_CATALOG
            .iter()
            .all(|s| !s.name.is_empty() && !s.description.is_empty()));
    }

    #[test]
    fn lookup_by_name_exact_and_containment() {
        assert_eq!(get_sticker_by_name("六六六").unwrap().sticker_id, "278");
        // query contains key
        assert_eq!(get_sticker_by_name("比心呀").unwrap().name, "比心");
        // key contains query
        assert_eq!(get_sticker_by_name("我想").unwrap().name, "我想开了");
        // whitespace trimmed
        assert_eq!(get_sticker_by_name("  酷 ").unwrap().sticker_id, "131");
        assert!(get_sticker_by_name("").is_none());
    }

    #[test]
    fn lookup_by_description_then_fuzzy() {
        // description containment (程序员 only appears in 头秃)
        assert_eq!(get_sticker_by_name("程序员").unwrap().name, "头秃");
        // fuzzy fallback: no exact/containment/description hit
        assert_eq!(get_sticker_by_name("厉害牛").unwrap().name, "六六六");
    }

    #[test]
    fn lookup_by_id() {
        assert_eq!(get_sticker_by_id("225").unwrap().name, "狗头");
        assert!(get_sticker_by_id("99999").is_none());
        assert!(get_sticker_by_id("  ").is_none());
    }

    #[test]
    fn search_empty_query_returns_catalog_head() {
        let top = search_stickers("", 3);
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].name, "六六六");
        assert_eq!(top[2].name, "害羞");
        let padded = search_stickers("   ", 2);
        assert_eq!(padded[0].name, "六六六");
    }

    #[test]
    fn search_ranks_exact_and_synonym_hits() {
        assert_eq!(search_stickers("六六六", 5)[0].name, "六六六");
        assert_eq!(search_stickers("doge 保命", 5)[0].name, "狗头");
        // sticker_id queries rank the matching sticker first
        assert_eq!(search_stickers("278", 5)[0].sticker_id, "278");
    }

    #[test]
    fn random_sticker_category_filters() {
        for _ in 0..8 {
            let sticker = get_random_sticker(Some("doge"));
            assert!(
                sticker.description.contains("doge") || sticker.name.contains("doge"),
                "category filter leaked: {:?}",
                sticker.name
            );
        }
        let any = get_random_sticker(None);
        assert!(STICKER_CATALOG.contains(any));
        let missing = get_random_sticker(Some("不存在的类别xyz"));
        assert!(STICKER_CATALOG.contains(missing));
    }

    #[test]
    fn build_sticker_msg_body_matches_wire_format() {
        let sticker = get_sticker_by_name("狗头").unwrap();
        let element = build_sticker_msg_body(sticker);
        assert_eq!(element.msg_type, "TIMFaceElem");
        assert_eq!(element.msg_content.index, 0);
        let value: serde_json::Value = serde_json::from_str(&element.msg_content.data).unwrap();
        assert_eq!(value["sticker_id"], "225");
        assert_eq!(value["package_id"], "1003");
        assert_eq!(value["width"], 128);
        assert_eq!(value["height"], 128);
        assert_eq!(value["formats"], "png");
        assert_eq!(value["name"], "狗头");
        // hermes json.dumps key order is preserved
        assert!(element
            .msg_content
            .data
            .starts_with("{\"sticker_id\":\"225\",\"package_id\":\"1003\","));
    }

    #[test]
    fn build_face_msg_body_legacy_index() {
        let element = build_face_msg_body(5, None);
        assert_eq!(element.msg_type, "TIMFaceElem");
        assert_eq!(element.msg_content.index, 5);
        assert!(element.msg_content.data.is_empty());
    }

    #[test]
    fn render_face_element_variants() {
        let with_name = proto::MsgContent {
            data: "{\"sticker_id\":\"225\",\"name\":\"狗头\"}".into(),
            ..Default::default()
        };
        assert_eq!(render_face_element(&with_name), "[emoji: 狗头]");
        // fallback: reverse-lookup by sticker_id when name is absent
        let id_only = proto::MsgContent {
            data: "{\"sticker_id\":\"225\"}".into(),
            ..Default::default()
        };
        assert_eq!(render_face_element(&id_only), "[emoji: 狗头]");
        let unknown_id = proto::MsgContent {
            data: "{\"sticker_id\":\"99999\"}".into(),
            ..Default::default()
        };
        assert_eq!(render_face_element(&unknown_id), "[emoji]");
        let garbage = proto::MsgContent {
            data: "not json".into(),
            ..Default::default()
        };
        assert_eq!(render_face_element(&garbage), "[emoji]");
        assert_eq!(
            render_face_element(&proto::MsgContent::default()),
            "[emoji]"
        );
    }

    #[test]
    fn extract_sticker_tags_splits_resolvable_names() {
        let reply = "好的\nSTICKER:狗头\n\nSTICKER:`ok`";
        let (text, names) = extract_sticker_tags(reply);
        assert_eq!(names, vec!["狗头".to_string(), "ok".to_string()]);
        assert_eq!(text, "好的");
        // hermes semantics: the fuzzy fallback always ranks the full
        // catalog, so any non-empty name resolves to its best match.
        let (fuzzy_text, fuzzy_names) = extract_sticker_tags("STICKER:不存在的贴纸");
        assert_eq!(fuzzy_names.len(), 1);
        assert!(fuzzy_text.is_empty());
        let (plain, none) = extract_sticker_tags("没有贴纸");
        assert!(none.is_empty());
        assert_eq!(plain, "没有贴纸");
        // empty candidate stays in the text
        let (kept, kept_names) = extract_sticker_tags("STICKER:");
        assert!(kept_names.is_empty());
        assert_eq!(kept, "STICKER:");
    }
}
