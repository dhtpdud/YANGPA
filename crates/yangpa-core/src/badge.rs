//! 도전과제 뱃지(decisions.md "도전과제 뱃지").
//!
//! - 모은 뱃지는 비트([`Badge::bit`])로 싣습니다. 모르는 비트는 무시하므로 뱃지를 더해도 옛 빌드가 카드를 풉니다.
//! - 카드는 신원 키로 서명합니다([`SignedBadgeCard`]). 프로필과 따로 두는 까닭: 프로필 서명은 postcard
//!   `(user, profile)` 바이트라 필드를 더하면 옛 빌드가 새 프로필을 거부함.
//! - 원년멤버만 개발자 계정이 서명한 증서([`FounderGrant`])로 받는 쪽이 확인합니다. 나머지는 본인 데몬이 계산해 알리는
//!   값이라 고친 클라이언트는 지어낼 수 있습니다(서버가 없음).

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::ids::UserId;
use crate::sign::{Sig, SignDomain, SigningKeypair, verify};

/// 원년멤버 증서를 서명하는 개발자 계정. 비어 있으면 아무도 원년멤버를 주지 못합니다.
pub const DEVELOPER: Option<UserId> = None;

/// 이름 앞뒤에 보일 수 있는 뱃지 수.
pub const MAX_SHOWN: usize = 2;

/// 뱃지. 번호(비트 자리)는 서명된 카드에 실리므로 바꾸지 않습니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Badge {
    Founder,
    Support3,
    Support14,
    Support50,
    Friends5,
    Friends10,
    Friends20,
    Friends50,
    Friends100,
    Crew10,
    Crew100,
    Crew1000,
    Crew10000,
    Seed1Gb,
    Seed10Gb,
    Seed100Gb,
    Seed1Tb,
    Seed10Tb,
    Seed100Tb,
    Seed1Pb,
    Voice1h,
    Voice10h,
    Voice100h,
    Voice1000h,
    Voice10000h,
    Screen1h,
    Screen10h,
    Screen100h,
    Screen1000h,
    Screen10000h,
}

/// 기준 → 뱃지 표(낮은 기준부터).
pub const SUPPORT_CENTS: [(u64, Badge); 3] = [
    (300, Badge::Support3),
    (1400, Badge::Support14),
    (5000, Badge::Support50),
];
pub const FRIENDS: [(u64, Badge); 5] = [
    (5, Badge::Friends5),
    (10, Badge::Friends10),
    (20, Badge::Friends20),
    (50, Badge::Friends50),
    (100, Badge::Friends100),
];
pub const CREW_MEMBERS: [(u64, Badge); 4] = [
    (10, Badge::Crew10),
    (100, Badge::Crew100),
    (1000, Badge::Crew1000),
    (10_000, Badge::Crew10000),
];
/// 시딩 바이트(앱의 크기 표시와 같은 10진 단위).
pub const SEED_BYTES: [(u64, Badge); 7] = [
    (1_000_000_000, Badge::Seed1Gb),
    (10_000_000_000, Badge::Seed10Gb),
    (100_000_000_000, Badge::Seed100Gb),
    (1_000_000_000_000, Badge::Seed1Tb),
    (10_000_000_000_000, Badge::Seed10Tb),
    (100_000_000_000_000, Badge::Seed100Tb),
    (1_000_000_000_000_000, Badge::Seed1Pb),
];
/// 다른 사람과 함께한 통화 시간(초).
pub const VOICE_SECS: [(u64, Badge); 5] = [
    (3_600, Badge::Voice1h),
    (36_000, Badge::Voice10h),
    (360_000, Badge::Voice100h),
    (3_600_000, Badge::Voice1000h),
    (36_000_000, Badge::Voice10000h),
];
/// 다른 사람이 있는 통화에서 내 화면을 공유한 시간(초).
pub const SCREEN_SECS: [(u64, Badge); 5] = [
    (3_600, Badge::Screen1h),
    (36_000, Badge::Screen10h),
    (360_000, Badge::Screen100h),
    (3_600_000, Badge::Screen1000h),
    (36_000_000, Badge::Screen10000h),
];

impl Badge {
    pub const ALL: [Badge; 30] = [
        Badge::Founder,
        Badge::Support3,
        Badge::Support14,
        Badge::Support50,
        Badge::Friends5,
        Badge::Friends10,
        Badge::Friends20,
        Badge::Friends50,
        Badge::Friends100,
        Badge::Crew10,
        Badge::Crew100,
        Badge::Crew1000,
        Badge::Crew10000,
        Badge::Seed1Gb,
        Badge::Seed10Gb,
        Badge::Seed100Gb,
        Badge::Seed1Tb,
        Badge::Seed10Tb,
        Badge::Seed100Tb,
        Badge::Seed1Pb,
        Badge::Voice1h,
        Badge::Voice10h,
        Badge::Voice100h,
        Badge::Voice1000h,
        Badge::Voice10000h,
        Badge::Screen1h,
        Badge::Screen10h,
        Badge::Screen100h,
        Badge::Screen1000h,
        Badge::Screen10000h,
    ];

    pub fn bit(self) -> u8 {
        self as u8
    }

    pub fn from_bit(bit: u8) -> Option<Badge> {
        Badge::ALL.get(usize::from(bit)).copied()
    }

    pub fn mask(self) -> u64 {
        1 << self.bit()
    }

    /// 비트 집합의 아는 뱃지(번호 순).
    pub fn list(bits: u64) -> Vec<Badge> {
        Badge::ALL
            .into_iter()
            .filter(|b| bits & b.mask() != 0)
            .collect()
    }

    /// `value`가 넘은 기준의 뱃지 비트.
    pub fn reached(table: &[(u64, Badge)], value: u64) -> u64 {
        table
            .iter()
            .filter(|(min, _)| value >= *min)
            .fold(0, |acc, (_, b)| acc | b.mask())
    }
}

/// 이름 앞(`after == false`)이나 뒤에 보일 뱃지.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownBadge {
    pub bit: u8,
    pub after: bool,
}

/// 개발자 계정([`DEVELOPER`])이 그 사용자에게 준 원년멤버 증서.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FounderGrant {
    pub user: UserId,
    pub sig: Sig,
}

impl FounderGrant {
    pub fn sign(developer: &SigningKeypair, user: UserId) -> Result<FounderGrant, CoreError> {
        let bytes = postcard::to_stdvec(&user)?;
        Ok(FounderGrant {
            user,
            sig: developer.sign(SignDomain::FounderGrant, &bytes),
        })
    }

    pub fn verify(&self, developer: &UserId) -> Result<(), CoreError> {
        let bytes = postcard::to_stdvec(&self.user)?;
        verify(
            developer.as_bytes(),
            SignDomain::FounderGrant,
            &bytes,
            &self.sig,
        )
    }
}

/// 사용자의 뱃지 카드. 모은 뱃지는 한 번 얻으면 남습니다(도전과제).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeCard {
    /// 바꿀 때마다 오릅니다(밀리초 시각 이상). 받는 쪽은 더 큰 판만 받습니다.
    pub version: u64,
    /// 모은 뱃지 비트. 원년멤버 비트는 `founder` 증서가 맞을 때만 인정합니다.
    pub earned: u64,
    /// 이름 앞뒤에 보일 뱃지([`MAX_SHOWN`]개까지, 모은 것 중에서).
    pub shown: Vec<ShownBadge>,
    pub founder: Option<FounderGrant>,
}

impl BadgeCard {
    pub fn sign(self, identity: &SigningKeypair) -> Result<SignedBadgeCard, CoreError> {
        let user = UserId(identity.public());
        let bytes = postcard::to_stdvec(&(&user, &self))?;
        let sig = identity.sign(SignDomain::Badges, &bytes);
        Ok(SignedBadgeCard {
            user,
            card: self,
            sig,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedBadgeCard {
    pub user: UserId,
    pub card: BadgeCard,
    pub sig: Sig,
}

impl SignedBadgeCard {
    /// 서명과 모양(보일 뱃지 수)만 봅니다. 원년멤버 증서는 [`Self::earned`]가 봅니다.
    pub fn verify(&self) -> Result<(), CoreError> {
        if self.card.shown.len() > MAX_SHOWN {
            return Err(CoreError::InvalidEncoding);
        }
        let bytes = postcard::to_stdvec(&(&self.user, &self.card))?;
        verify(self.user.as_bytes(), SignDomain::Badges, &bytes, &self.sig)
    }

    /// 인정하는 뱃지 비트: 원년멤버는 이 사용자에게 준 개발자 증서가 맞을 때만.
    pub fn earned(&self, developer: Option<&UserId>) -> u64 {
        let founder =
            self.card.founder.as_ref().is_some_and(|g| {
                g.user == self.user && developer.is_some_and(|d| g.verify(d).is_ok())
            });
        let bits = self.card.earned & !Badge::Founder.mask();
        if founder {
            bits | Badge::Founder.mask()
        } else {
            bits
        }
    }

    /// 이름 앞뒤에 보일 뱃지(인정하는 것만, 아는 것만).
    pub fn shown(&self, developer: Option<&UserId>) -> Vec<(Badge, bool)> {
        let earned = self.earned(developer);
        self.card
            .shown
            .iter()
            .filter_map(|s| Badge::from_bit(s.bit).map(|b| (b, s.after)))
            .filter(|(b, _)| earned & b.mask() != 0)
            .take(MAX_SHOWN)
            .collect()
    }
}

/// 이름에 쓸 수 없는 그림 문자(이모지)가 있는지. 뱃지와 헷갈리지 않게 이름·별명에서 막습니다.
/// 앱의 `hasEmoji`(`apps/desktop/src/lib/emoji.ts`)와 같은 범위입니다. ©®™·화살표 같은 글자 기호는 둡니다.
pub fn has_emoji(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            u32::from(c),
            0x1F000..=0x1FAFF   // 마작·카드·둘러싼 문자(국기 글자 포함)·그림 문자·이모티콘·교통·보충 기호
            | 0x1FC00..=0x1FFFD
            | 0x2600..=0x27BF   // 여러 기호·딩뱃(☕⛺★♥ 등)
            | 0x231A..=0x231B
            | 0x2328
            | 0x23CF
            | 0x23E9..=0x23FA
            | 0x2B05..=0x2B07
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x3030
            | 0x303D
            | 0x3297
            | 0x3299
            | 0x20E3            // 키캡
            | 0xFE0F            // 이모지 모양 선택자
            | 0xE0020..=0xE007F // 깃발 태그
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_bits_are_stable() {
        for (i, b) in Badge::ALL.iter().enumerate() {
            assert_eq!(usize::from(b.bit()), i);
            assert_eq!(Badge::from_bit(b.bit()), Some(*b));
        }
        assert_eq!(Badge::Seed1Pb.bit(), 19);
        assert_eq!(Badge::Voice1h.bit(), 20);
        assert_eq!(Badge::Screen10000h.bit(), 29);
        assert_eq!(Badge::from_bit(30), None);
        assert_eq!(
            Badge::list(Badge::reached(&VOICE_SECS, 36_000)),
            vec![Badge::Voice1h, Badge::Voice10h]
        );
        assert_eq!(
            serde_json::to_string(&Badge::Screen10000h).unwrap(),
            "\"screen10000h\""
        );
        // 기준 표는 기준과 같거나 넘으면 얻습니다.
        assert_eq!(Badge::reached(&FRIENDS, 4), 0);
        assert_eq!(
            Badge::reached(&FRIENDS, 10),
            Badge::Friends5.mask() | Badge::Friends10.mask()
        );
        assert_eq!(
            Badge::list(Badge::reached(&SEED_BYTES, u64::MAX)).len(),
            SEED_BYTES.len()
        );
        assert_eq!(
            serde_json::to_string(&Badge::Seed100Gb).unwrap(),
            "\"seed100gb\""
        );
    }

    #[test]
    fn card_signature_and_founder_grant() {
        let dev = SigningKeypair::generate();
        let dev_id = UserId(dev.public());
        let me = SigningKeypair::generate();
        let user = UserId(me.public());
        let unknown_bit = 1 << 40;
        let card = BadgeCard {
            version: 5,
            earned: Badge::Founder.mask() | Badge::Friends5.mask() | unknown_bit,
            shown: vec![
                ShownBadge {
                    bit: Badge::Founder.bit(),
                    after: false,
                },
                ShownBadge {
                    bit: Badge::Friends5.bit(),
                    after: true,
                },
            ],
            founder: Some(FounderGrant::sign(&dev, user).unwrap()),
        }
        .sign(&me)
        .unwrap();
        card.verify().unwrap();
        let bin = postcard::to_stdvec(&card).unwrap();
        assert_eq!(postcard::from_bytes::<SignedBadgeCard>(&bin).unwrap(), card);

        // 증서는 개발자 키로만 인정합니다. 모르는 비트는 남겨 두되 목록에는 나오지 않습니다.
        assert_ne!(card.earned(Some(&dev_id)) & Badge::Founder.mask(), 0);
        assert_eq!(card.earned(None) & Badge::Founder.mask(), 0);
        assert_eq!(card.earned(Some(&user)) & Badge::Founder.mask(), 0);
        assert_eq!(
            Badge::list(card.earned(Some(&dev_id))),
            vec![Badge::Founder, Badge::Friends5]
        );
        assert_eq!(card.shown(None), vec![(Badge::Friends5, true)]);
        assert_eq!(card.shown(Some(&dev_id)).len(), 2);

        // 남의 증서를 붙여도 인정하지 않습니다.
        let other = UserId(SigningKeypair::generate().public());
        let stolen = BadgeCard {
            version: 6,
            earned: Badge::Founder.mask(),
            shown: vec![],
            founder: Some(FounderGrant::sign(&dev, other).unwrap()),
        }
        .sign(&me)
        .unwrap();
        assert_eq!(stolen.earned(Some(&dev_id)), 0);

        // 서명 뒤 바꾼 카드, 보일 뱃지가 너무 많은 카드는 거부합니다.
        let mut forged = card.clone();
        forged.card.earned |= Badge::Support50.mask();
        assert!(forged.verify().is_err());
        let crowded = BadgeCard {
            shown: vec![
                ShownBadge {
                    bit: 4,
                    after: true
                };
                3
            ],
            ..Default::default()
        }
        .sign(&me)
        .unwrap();
        assert!(crowded.verify().is_err());
    }

    /// 앱의 `hasEmoji` 시험(`src/lib/emoji.test.ts`)과 같은 예.
    #[test]
    fn emoji_in_names() {
        for s in [
            "철수🧅",
            "☕",
            "⛺",
            "★별",
            "♥",
            "🇰🇷",
            "1\u{FE0F}\u{20E3}",
            "👨‍👩‍👧",
            "⭐",
            "⌚",
        ] {
            assert!(has_emoji(s), "{s}");
        }
        for s in [
            "철수",
            "Alice_99",
            "日本語",
            "©®™",
            "→←",
            "a\u{200D}b",
            "‼",
            "(・ω・)",
            "Ωμ",
            "١٢٣",
        ] {
            assert!(!has_emoji(s), "{s}");
        }
    }
}
