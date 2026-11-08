//! 익명 채널의 가명: 같은 채널에서 같은 사람은 늘 같은 '형용사 + 동물' 이름(예: `용감한 수달`)으로 보입니다.
//!
//! 이름은 (공간, 채널, 사용자)에서 정해지므로 모든 멤버의 화면에서 같고, 채널이 다르면 같은 사람도 다른 이름입니다.
//! 익명은 화면에서만 가리는 것이라(decisions.md, 데이터에는 서명자가 그대로) 이름에 따로 비밀을 섞지 않습니다.

use yangpa_core::ids::{ChannelId, SpaceId, UserId};

const ADJECTIVES: &[&str] = &[
    "용감한",
    "졸린",
    "배고픈",
    "수줍은",
    "느긋한",
    "재빠른",
    "조용한",
    "씩씩한",
    "엉뚱한",
    "다정한",
    "신나는",
    "꼼꼼한",
    "귀여운",
    "멋진",
    "날쌘",
    "똑똑한",
    "행복한",
    "차분한",
    "활발한",
    "우아한",
    "겸손한",
    "대담한",
    "발랄한",
    "상냥한",
    "성실한",
    "소심한",
    "수상한",
    "시원한",
    "따뜻한",
    "포근한",
    "반짝이는",
    "노래하는",
    "춤추는",
    "꿈꾸는",
    "웃는",
    "달리는",
    "날아가는",
    "헤엄치는",
    "숨어 있는",
    "떠도는",
    "지친",
    "들뜬",
    "심심한",
    "궁금한",
    "깐깐한",
    "너그러운",
    "부지런한",
    "게으른",
    "엉성한",
    "단단한",
    "말랑한",
    "푸른",
    "붉은",
    "노란",
    "하얀",
    "까만",
    "은빛",
    "금빛",
    "작은",
    "커다란",
    "동그란",
    "길쭉한",
    "폭신한",
    "매콤한",
    "달콤한",
    "새콤한",
    "고소한",
    "향긋한",
    "수다스러운",
    "과묵한",
    "명랑한",
    "침착한",
    "엉큼한",
    "순한",
    "사나운",
    "용맹한",
    "영리한",
    "신비한",
    "낭만적인",
    "덤덤한",
];

const ANIMALS: &[&str] = &[
    "수달",
    "고양이",
    "강아지",
    "너구리",
    "여우",
    "토끼",
    "다람쥐",
    "판다",
    "펭귄",
    "부엉이",
    "올빼미",
    "고슴도치",
    "햄스터",
    "오리",
    "거북이",
    "코알라",
    "캥거루",
    "기린",
    "코끼리",
    "사자",
    "호랑이",
    "곰",
    "늑대",
    "사슴",
    "알파카",
    "라마",
    "독수리",
    "참새",
    "까치",
    "비둘기",
    "앵무새",
    "두루미",
    "백조",
    "홍학",
    "돌고래",
    "고래",
    "물개",
    "해달",
    "문어",
    "오징어",
    "해파리",
    "불가사리",
    "게",
    "새우",
    "가재",
    "개구리",
    "도롱뇽",
    "도마뱀",
    "카멜레온",
    "이구아나",
    "나무늘보",
    "미어캣",
    "라쿤",
    "족제비",
    "담비",
    "두더지",
    "박쥐",
    "고릴라",
    "원숭이",
    "하마",
    "코뿔소",
    "얼룩말",
    "낙타",
    "양",
    "염소",
    "젖소",
    "돼지",
    "병아리",
    "망아지",
    "청설모",
    "반딧불이",
    "나비",
    "꿀벌",
    "무당벌레",
    "달팽이",
    "펠리컨",
    "까마귀",
    "갈매기",
    "공작",
    "치타",
];

/// 그 채널에서 그 사람의 가명.
pub(crate) fn name(space: &SpaceId, channel: &ChannelId, user: &UserId) -> String {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v1 anonymous name");
    h.update(&space.0);
    h.update(&channel.0);
    h.update(&user.0);
    let d = h.finalize();
    let b = d.as_bytes();
    let a = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize % ADJECTIVES.len();
    let n = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize % ANIMALS.len();
    format!("{} {}", ADJECTIVES[a], ANIMALS[n])
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn words_are_unique() {
        assert_eq!(
            ADJECTIVES.iter().collect::<HashSet<_>>().len(),
            ADJECTIVES.len()
        );
        assert_eq!(ANIMALS.iter().collect::<HashSet<_>>().len(), ANIMALS.len());
    }

    #[test]
    fn stable_per_channel_and_differs_between_people() {
        let (s, c, c2) = (SpaceId::random(), ChannelId::random(), ChannelId::random());
        let users: Vec<UserId> = (0..20).map(|_| UserId::random()).collect();
        assert_eq!(name(&s, &c, &users[0]), name(&s, &c, &users[0]));
        let names: HashSet<String> = users.iter().map(|u| name(&s, &c, u)).collect();
        // 20명이면 겹칠 확률이 낮지만 0은 아니므로 대부분 다르기만 확인합니다.
        assert!(names.len() >= 17, "{names:?}");
        let moved = users
            .iter()
            .filter(|u| name(&s, &c, u) != name(&s, &c2, u))
            .count();
        assert!(moved >= 17);
    }
}
