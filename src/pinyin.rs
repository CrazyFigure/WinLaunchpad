//! 汉字拼音映射与中英文混合字母排序支持（零运行时外部依赖）。
//!
//! `pinyin_table.bin` 收录 Unicode 基本区全部 20,992 个汉字（`U+4E00..=U+9FFF`）
//! 的小端 `u16` 拼音音节索引（0 表示无音节，1..=407 对应 `SYLLABLES[id - 1]`）。

use std::cmp::Ordering;

use windows::Win32::Globalization::{CompareStringW, CSTR_GREATER_THAN, CSTR_LESS_THAN};

const PINYIN_TABLE: &[u8] = include_bytes!("pinyin_table.bin");
const CJK_START: u32 = 0x4E00;
const CJK_END: u32 = 0x9FFF;
/// zh-CN 区域标识（固定使用中文拼音顺序做同音字兜底比较，避免英文系统区域下退化）。
const LCID_ZH_CN: u32 = 0x0804;

const SYLLABLES: &[&str] = &[
    "a", "ai", "an", "ang", "ao", "ba", "bai", "ban", "bang", "bao", "bei", "ben", "beng", "bi",
    "bian", "biao", "bie", "bin", "bing", "bo", "bu", "ca", "cai", "can", "cang", "cao", "ce",
    "cen", "ceng", "cha", "chai", "chan", "chang", "chao", "che", "chen", "cheng", "chi", "chong",
    "chou", "chu", "chua", "chuai", "chuan", "chuang", "chui", "chun", "chuo", "ci", "cong", "cou",
    "cu", "cuan", "cui", "cun", "cuo", "da", "dai", "dan", "dang", "dao", "de", "dei", "den",
    "deng", "di", "dia", "dian", "diao", "die", "ding", "diu", "dong", "dou", "du", "duan", "dui",
    "dun", "duo", "e", "ei", "en", "eng", "er", "fa", "fan", "fang", "fei", "fen", "feng", "fiao",
    "fo", "fou", "fu", "ga", "gai", "gan", "gang", "gao", "ge", "gei", "gen", "geng", "gong",
    "gou", "gu", "gua", "guai", "guan", "guang", "gui", "gun", "guo", "ha", "hai", "han", "hang",
    "hao", "he", "hei", "hen", "heng", "hong", "hou", "hu", "hua", "huai", "huan", "huang", "hui",
    "hun", "huo", "ji", "jia", "jian", "jiang", "jiao", "jie", "jin", "jing", "jiong", "jiu", "ju",
    "juan", "jue", "jun", "ka", "kai", "kan", "kang", "kao", "ke", "kei", "ken", "keng", "kong",
    "kou", "ku", "kua", "kuai", "kuan", "kuang", "kui", "kun", "kuo", "la", "lai", "lan", "lang",
    "lao", "le", "lei", "leng", "li", "lian", "liang", "liao", "lie", "lin", "ling", "liu", "lo",
    "long", "lou", "lu", "luan", "lue", "lun", "luo", "ma", "mai", "man", "mang", "mao", "me",
    "mei", "men", "meng", "mi", "mian", "miao", "mie", "min", "ming", "miu", "mo", "mou", "mu",
    "na", "nai", "nan", "nang", "nao", "ne", "nei", "nen", "neng", "ni", "nian", "niang", "niao",
    "nie", "nin", "ning", "niu", "nong", "nou", "nu", "nuan", "nue", "nun", "nuo", "o", "ou", "pa",
    "pai", "pan", "pang", "pao", "pei", "pen", "peng", "pi", "pian", "piao", "pie", "pin", "ping",
    "po", "pou", "pu", "qi", "qia", "qian", "qiang", "qiao", "qie", "qin", "qing", "qiong", "qiu",
    "qu", "quan", "que", "qun", "ran", "rang", "rao", "re", "ren", "reng", "ri", "rong", "rou",
    "ru", "ruan", "rui", "run", "ruo", "sa", "sai", "san", "sang", "sao", "se", "sen", "seng",
    "sha", "shai", "shan", "shang", "shao", "she", "shen", "sheng", "shi", "shou", "shu", "shua",
    "shuai", "shuan", "shuang", "shui", "shun", "shuo", "si", "song", "sou", "su", "suan", "sui",
    "sun", "suo", "ta", "tai", "tan", "tang", "tao", "te", "teng", "ti", "tian", "tiao", "tie",
    "ting", "tong", "tou", "tu", "tuan", "tui", "tun", "tuo", "wa", "wai", "wan", "wang", "wei",
    "wen", "weng", "wo", "wu", "xi", "xia", "xian", "xiang", "xiao", "xie", "xin", "xing", "xiong",
    "xiu", "xu", "xuan", "xue", "xun", "ya", "yan", "yang", "yao", "ye", "yi", "yin", "ying",
    "yo", "yong", "you", "yu", "yuan", "yue", "yun", "za", "zai", "zan", "zang", "zao", "ze",
    "zei", "zen", "zeng", "zha", "zhai", "zhan", "zhang", "zhao", "zhe", "zhei", "zhen", "zheng",
    "zhi", "zhong", "zhou", "zhu", "zhua", "zhuai", "zhuan", "zhuang", "zhui", "zhun", "zhuo",
    "zi", "zong", "zou", "zu", "zuan", "zui", "zun", "zuo",
];

/// 查询单个字符对应的拼音音节（无声调小写 ASCII）；非收录汉字返回 `None`。
#[inline]
pub fn char_pinyin(ch: char) -> Option<&'static str> {
    if ch == '\u{3007}' {
        return Some("ling");
    }
    let cp = ch as u32;
    if !(CJK_START..=CJK_END).contains(&cp) {
        return None;
    }
    let off = ((cp - CJK_START) as usize) * 2;
    let sid = u16::from_le_bytes([PINYIN_TABLE[off], PINYIN_TABLE[off + 1]]) as usize;
    if sid == 0 {
        None
    } else {
        SYLLABLES.get(sid - 1).copied()
    }
}

/// 生成用于中英文混合按字母顺序比较的标准化键：
/// - 中文字符展开为无声调拼音小写字母；
/// - 全角数字/字母归约为半角 ASCII 小写；
/// - 英文统一小写。
pub fn mixed_sort_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if let Some(py) = char_pinyin(ch) {
            out.push_str(py);
        } else if ('\u{FF10}'..='\u{FF19}').contains(&ch)
            || ('\u{FF21}'..='\u{FF3A}').contains(&ch)
            || ('\u{FF41}'..='\u{FF5A}').contains(&ch)
        {
            let ascii = char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch);
            out.push(ascii.to_ascii_lowercase());
        } else if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase());
        } else {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
    out
}

/// 中英文混合按字母顺序比较：主排序键为汉字拼音 + 英文小写字母序列，
/// 同音或同字母时回退 `zh-CN` 区域比较与原始码位比较以保证全序稳定。
pub fn cmp_mixed_names(a: &str, b: &str) -> Ordering {
    let ka = mixed_sort_key(a);
    let kb = mixed_sort_key(b);
    let primary = ka.cmp(&kb);
    if primary != Ordering::Equal {
        return primary;
    }
    let la: Vec<u16> = a.encode_utf16().collect();
    let lb: Vec<u16> = b.encode_utf16().collect();
    let r = unsafe { CompareStringW(LCID_ZH_CN, 1, &la, &lb) };
    if r == CSTR_LESS_THAN {
        Ordering::Less
    } else if r == CSTR_GREATER_THAN {
        Ordering::Greater
    } else {
        a.cmp(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_size_and_indices_are_valid() {
        let expected_entries = (CJK_END - CJK_START + 1) as usize;
        assert_eq!(PINYIN_TABLE.len(), expected_entries * 2);
        for chunk in PINYIN_TABLE.chunks_exact(2) {
            let sid = u16::from_le_bytes([chunk[0], chunk[1]]) as usize;
            assert!(sid <= SYLLABLES.len(), "音节索引越界: {sid}");
        }
    }

    #[test]
    fn common_hanzi_map_to_expected_pinyin() {
        assert_eq!(char_pinyin('微'), Some("wei"));
        assert_eq!(char_pinyin('信'), Some("xin"));
        assert_eq!(char_pinyin('钉'), Some("ding"));
        assert_eq!(char_pinyin('支'), Some("zhi"));
        assert_eq!(char_pinyin('付'), Some("fu"));
        assert_eq!(char_pinyin('宝'), Some("bao"));
        assert_eq!(char_pinyin('啊'), Some("a"));
        assert_eq!(char_pinyin('算'), Some("suan"));
        assert_eq!(char_pinyin('〇'), Some("ling"));
        assert_eq!(char_pinyin('龙'), Some("long"));
        assert_eq!(char_pinyin('龍'), Some("long"));
        assert_eq!(char_pinyin('A'), None);
    }

    #[test]
    fn mixed_chinese_and_english_sort_alphabetically() {
        let mut names = [
            "Zoom",
            "微信",
            "Chrome",
            "钉钉",
            "7-Zip",
            "爱奇艺",
            "计算器",
            "Baidu",
            "百度网盘",
            "WebStorm",
            "网易云音乐",
            "Edge",
            "支付宝",
        ];
        names.sort_by(|a, b| cmp_mixed_names(a, b));
        assert_eq!(
            names,
            [
                "7-Zip",
                "爱奇艺",
                "Baidu",
                "百度网盘",
                "Chrome",
                "钉钉",
                "Edge",
                "计算器",
                "网易云音乐",
                "WebStorm",
                "微信",
                "支付宝",
                "Zoom",
            ]
        );
    }

    #[test]
    fn homophones_and_case_have_deterministic_order() {
        assert_eq!(cmp_mixed_names("Alpha", "beta"), Ordering::Less);
        assert_eq!(cmp_mixed_names("alpha", "Beta"), Ordering::Less);
        // 大小写相同音键时提供确定性全序，且正反比较严格互逆
        let c1 = cmp_mixed_names("Alpha", "alpha");
        let c2 = cmp_mixed_names("alpha", "Alpha");
        assert_ne!(c1, Ordering::Equal);
        assert_eq!(c1, c2.reverse());
        // "Baidu" 与 "百度" 主键同为 "baidu"，按 zh-CN 规则稳定排序
        let o1 = cmp_mixed_names("Baidu", "百度");
        let o2 = cmp_mixed_names("百度", "Baidu");
        assert_ne!(o1, Ordering::Equal);
        assert_eq!(o1, o2.reverse());
    }
}
