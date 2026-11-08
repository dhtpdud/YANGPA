// 이모지 선택기용 유니코드 이모지 목록(검색어는 한국어·영어). 전체 목록 대신 자주 쓰는 것만 둡니다.

export interface UnicodeEmoji {
  char: string;
  keywords: string;
}

export interface EmojiCategory {
  id: string;
  label: string;
  emojis: UnicodeEmoji[];
}

// "이모지 검색어|이모지 검색어|…" 형식으로 짧게 적습니다.
const RAW: Array<[string, string, string]> = [
  [
    'smileys',
    '표정',
    '😀 웃음 smile grin|😃 웃음 기쁨 happy|😄 활짝 smile|😁 히죽 grin|😆 빵터짐 laugh|😅 진땀 sweat|🤣 데굴데굴 rofl|😂 눈물웃음 joy lol|🙂 미소 slight|🙃 거꾸로 upside|😉 윙크 wink|😊 수줍 blush|😇 천사 angel|🥰 사랑 love hearts|😍 하트눈 heart eyes|🤩 반함 star|😘 뽀뽀 kiss|😋 맛있 yum|😛 메롱 tongue|😜 장난 wink tongue|🤪 엉뚱 zany|🤑 돈 money|🤗 포옹 hug|🤭 쉿웃음 giggle|🤫 쉿 shush|🤔 생각 think|🤐 입다물 zip|🤨 의심 raised|😐 무표정 neutral|😑 무덤덤 expressionless|😶 말없음 silent|😏 능글 smirk|😒 불만 unamused|🙄 눈굴림 eyeroll|😬 어색 grimace|😌 안도 relieved|😔 시무룩 pensive|😪 졸림 sleepy|🤤 침 drool|😴 잠 sleep|😷 마스크 mask sick|🤒 열 fever|🤕 다침 hurt|🤢 메스꺼움 nausea|🤮 토 vomit|🥵 더움 hot|🥶 추움 cold|🥴 어지러움 woozy|😵 기절 dizzy|🤯 충격 mindblown|🤠 카우보이 cowboy|🥳 파티 party|😎 선글라스 cool|🤓 범생 nerd|🧐 관찰 monocle|😕 혼란 confused|😟 걱정 worried|🙁 슬픔 frown|😮 놀람 wow|😯 헉 hushed|😲 깜짝 astonished|😳 당황 flushed|🥺 부탁 pleading|😦 어 frowning|😧 고뇌 anguished|😨 겁 fearful|😰 식은땀 anxious|😥 실망 sad relieved|😢 울음 cry|😭 엉엉 sob|😱 비명 scream|😖 괴로움 confounded|😣 버팀 persevere|😞 낙담 disappointed|😓 땀 sweat|😩 지침 weary|😫 피곤 tired|🥱 하품 yawn|😤 씩씩 triumph|😡 화남 angry rage|😠 성남 angry|🤬 욕 cursing|😈 악마 devil|💀 해골 skull dead|💩 똥 poop|🤡 광대 clown|👻 유령 ghost|👽 외계인 alien|🤖 로봇 robot',
  ],
  [
    'gestures',
    '손짓·사람',
    '👋 안녕 wave|🤚 손등 raised back|✋ 손 hand|🖖 벌칸 vulcan|👌 오케이 ok|🤌 이탈리아 pinched|🤏 조금 pinch|✌️ 브이 victory|🤞 행운 crossed|🤟 사랑해 love you|🤘 록 rock|🤙 전화 call|👈 왼쪽 left|👉 오른쪽 right|👆 위 up|👇 아래 down|☝️ 하나 point|👍 좋아요 따봉 thumbs up like|👎 싫어요 thumbs down|✊ 주먹 fist|👊 펀치 punch|🤛 왼주먹 fist|🤜 오른주먹 fist|👏 박수 clap|🙌 만세 hooray|👐 펼친손 open|🤲 받들기 palms|🤝 악수 handshake|🙏 부탁 감사 pray thanks|✍️ 쓰기 write|💪 힘 muscle|🧠 뇌 brain|👀 눈 eyes look|👁️ 눈하나 eye|👅 혀 tongue|👄 입 mouth|🫡 경례 salute|🫠 녹음 melting|🫶 하트손 heart hands',
  ],
  [
    'hearts',
    '하트·기호',
    '❤️ 하트 빨강 heart red|🧡 주황하트 orange|💛 노랑하트 yellow|💚 초록하트 green|💙 파랑하트 blue|💜 보라하트 purple|🖤 검정하트 black|🤍 흰하트 white|🤎 갈색하트 brown|💔 상심 broken|❣️ 느낌표하트|💕 두하트 two hearts|💞 회전하트|💓 두근 beating|💗 커지는하트|💖 반짝하트 sparkling|💘 화살하트 cupid|💝 선물하트|💯 백점 hundred|💢 분노 anger|💥 쾅 boom|💫 어지럼 dizzy|💦 땀방울 sweat|💨 휙 dash|💬 말풍선 speech|💭 생각풍선 thought|💤 쿨쿨 zzz|✅ 확인 check|❌ 엑스 cross no|⭕ 동그라미 circle|❗ 느낌표 exclamation|❓ 물음표 question|⚠️ 경고 warning|🔥 불 fire hot|✨ 반짝 sparkles|⭐ 별 star|🌟 빛나는별 glowing|⚡ 번개 zap|🎉 축하 party tada|🎊 색종이 confetti|🎁 선물 gift|🏆 트로피 trophy|🥇 금메달 gold|🎯 과녁 target|🔒 자물쇠 lock|🔑 열쇠 key|🔔 종 bell|📌 압정 pin|📎 클립 clip|🔗 링크 link',
  ],
  [
    'nature',
    '동물·자연',
    '🐶 강아지 dog|🐱 고양이 cat|🐭 쥐 mouse|🐹 햄스터 hamster|🐰 토끼 rabbit|🦊 여우 fox|🐻 곰 bear|🐼 판다 panda|🐨 코알라 koala|🐯 호랑이 tiger|🦁 사자 lion|🐮 소 cow|🐷 돼지 pig|🐸 개구리 frog|🐵 원숭이 monkey|🐔 닭 chicken|🐧 펭귄 penguin|🐦 새 bird|🐤 병아리 chick|🦆 오리 duck|🦉 부엉이 owl|🐺 늑대 wolf|🐗 멧돼지 boar|🐴 말 horse|🦄 유니콘 unicorn|🐝 벌 bee|🐛 애벌레 bug|🦋 나비 butterfly|🐌 달팽이 snail|🐢 거북 turtle|🐍 뱀 snake|🐙 문어 octopus|🦀 게 crab|🐟 물고기 fish|🐬 돌고래 dolphin|🐳 고래 whale|🌸 벚꽃 cherry blossom|🌹 장미 rose|🌻 해바라기 sunflower|🌱 새싹 seedling|🌲 나무 tree|🍀 네잎클로버 clover|🍁 단풍 maple|🌙 달 moon|☀️ 해 sun|⛅ 구름 cloud|🌧️ 비 rain|❄️ 눈 snow|🌈 무지개 rainbow|🌊 파도 wave',
  ],
  [
    'food',
    '음식',
    '🧅 양파 onion|🍎 사과 apple|🍊 귤 orange|🍋 레몬 lemon|🍌 바나나 banana|🍉 수박 watermelon|🍇 포도 grape|🍓 딸기 strawberry|🍑 복숭아 peach|🥝 키위 kiwi|🍅 토마토 tomato|🥑 아보카도 avocado|🌶️ 고추 pepper|🧄 마늘 garlic|🍞 빵 bread|🧀 치즈 cheese|🍳 계란 egg|🍗 치킨 chicken|🍖 고기 meat|🍔 햄버거 burger|🍟 감자튀김 fries|🍕 피자 pizza|🌭 핫도그 hotdog|🍜 라면 ramen|🍝 스파게티 spaghetti|🍣 초밥 sushi|🍙 주먹밥 rice ball|🍚 밥 rice|🍛 카레 curry|🥟 만두 dumpling|🍤 새우튀김 shrimp|🍦 아이스크림 icecream|🍩 도넛 donut|🍪 쿠키 cookie|🎂 케이크 cake|🍫 초콜릿 chocolate|🍿 팝콘 popcorn|☕ 커피 coffee|🍵 차 tea|🧋 버블티 boba|🍺 맥주 beer|🍻 건배 cheers|🍷 와인 wine|🥤 음료 drink',
  ],
  [
    'activity',
    '활동·사물',
    '⚽ 축구 soccer|🏀 농구 basketball|⚾ 야구 baseball|🎾 테니스 tennis|🏐 배구 volleyball|🎮 게임 game controller|🕹️ 조이스틱 joystick|🎲 주사위 dice|♟️ 체스 chess|🧩 퍼즐 puzzle|🎨 그림 art|🎬 영화 movie|🎤 마이크 mic|🎧 헤드폰 headphones|🎵 음표 music|🎸 기타 guitar|🎹 피아노 piano|📷 카메라 camera|📱 폰 phone|💻 노트북 laptop|🖥️ 모니터 desktop|⌨️ 키보드 keyboard|🖱️ 마우스 mouse|💾 디스크 floppy|📚 책 books|✏️ 연필 pencil|📝 메모 memo|📅 달력 calendar|📈 상승 chart up|📉 하락 chart down|🚀 로켓 rocket|✈️ 비행기 plane|🚗 자동차 car|🚲 자전거 bike|🏠 집 house|🏢 건물 building|⏰ 알람 alarm|⏳ 모래시계 hourglass|💡 전구 idea bulb|🔧 렌치 wrench|🔨 망치 hammer|🛡️ 방패 shield|🧪 실험 test|💊 약 pill|🧸 곰인형 teddy',
  ],
];

export const EMOJI_CATEGORIES: EmojiCategory[] = RAW.map(([id, label, raw]) => ({
  id,
  label,
  emojis: raw.split('|').map((entry) => {
    const sp = entry.indexOf(' ');
    return sp < 0 ? { char: entry, keywords: '' } : { char: entry.slice(0, sp), keywords: entry.slice(sp + 1) };
  }),
}));

export const ALL_UNICODE_EMOJI: UnicodeEmoji[] = EMOJI_CATEGORIES.flatMap((c) => c.emojis);

/** 빠른 반응 기본값. */
export const QUICK_REACTIONS = ['👍', '❤️', '😂', '😮', '😢', '🎉'];

export function searchUnicodeEmoji(query: string, limit = 80): UnicodeEmoji[] {
  const q = query.trim().toLowerCase();
  if (!q) return ALL_UNICODE_EMOJI.slice(0, limit);
  const out: UnicodeEmoji[] = [];
  for (const e of ALL_UNICODE_EMOJI) {
    if (e.keywords.toLowerCase().includes(q) || e.char === q) {
      out.push(e);
      if (out.length >= limit) break;
    }
  }
  return out;
}
