#![allow(dead_code, clippy::needless_range_loop)]

use tracing::{debug, warn};

const QUIC_IMAGE_TYPE_INVALID: u32 = 0;
const QUIC_IMAGE_TYPE_GRAY: u32 = 1;
const QUIC_IMAGE_TYPE_RGB16: u32 = 2;
const QUIC_IMAGE_TYPE_RGB24: u32 = 3;
const QUIC_IMAGE_TYPE_RGB32: u32 = 4;
const QUIC_IMAGE_TYPE_RGBA: u32 = 5;

const DEFEVOL: usize = 3;
const DEFWMIMAX: u32 = 6;
const DEFWMINEXT: u32 = 2048;
const DEFMAXCLEN: usize = 26;

const RGB32_PIXEL_PAD: usize = 3;
const RGB32_PIXEL_R: usize = 2;
const RGB32_PIXEL_G: usize = 1;
const RGB32_PIXEL_B: usize = 0;
const RGB32_PIXEL_SIZE: usize = 4;

const BPPMASK: [u32; 33] = [
    0x00000000, 0x00000001, 0x00000003, 0x00000007, 0x0000000f, 0x0000001f, 0x0000003f,
    0x0000007f, 0x000000ff, 0x000001ff, 0x000003ff, 0x000007ff, 0x00000fff, 0x00001fff,
    0x00003fff, 0x00007fff, 0x0000ffff, 0x0001ffff, 0x0003ffff, 0x0007ffff, 0x000fffff,
    0x001fffff, 0x003fffff, 0x007fffff, 0x00ffffff, 0x01ffffff, 0x03ffffff, 0x07ffffff,
    0x0fffffff, 0x1fffffff, 0x3fffffff, 0x7fffffff, 0xffffffff,
];

const BESTTRIGTAB: [[u32; 11]; 3] = [
    [550, 900, 800, 700, 500, 350, 300, 200, 180, 180, 160],
    [110, 550, 900, 800, 550, 400, 350, 250, 140, 160, 140],
    [100, 120, 550, 900, 700, 500, 400, 300, 220, 250, 160],
];

const J: [u8; 32] = [
    0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 9, 10, 11,
    12, 13, 14, 15,
];

const LZEROES: [u8; 256] = [
    8, 7, 6, 6, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const TABRAND_CHAOS: [u32; 256] = [
    0x02c57542, 0x35427717, 0x2f5a2153, 0x9244f155, 0x7bd26d07, 0x354c6052, 0x57329b28,
    0x2993868e, 0x6cd8808c, 0x147b46e0, 0x99db66af, 0xe32b4cac, 0x1b671264, 0x9d433486,
    0x62a4c192, 0x06089a4b, 0x9e3dce44, 0xdaabee13, 0x222425ea, 0xa46f331d, 0xcd589250,
    0x8bb81d7f, 0xc8b736b9, 0x35948d33, 0xd7ac7fd0, 0x5fbe2803, 0x2cfbc105, 0x013dbc4e,
    0x7a37820f, 0x39f88e9e, 0xedd58794, 0xc5076689, 0xfcada5a4, 0x64c2f46d, 0xb3ba3243,
    0x8974b4f9, 0x5a05aebd, 0x20afcd00, 0x39e2b008, 0x88a18a45, 0x600bde29, 0xf3971ace,
    0xf37b0a6b, 0x7041495b, 0x70b707ab, 0x06beffbb, 0x4206051f, 0xe13c4ee3, 0xc1a78327,
    0x91aa067c, 0x8295f72a, 0x732917a6, 0x1d871b4d, 0x4048f136, 0xf1840e7e, 0x6a6048c1,
    0x696cb71a, 0x7ff501c3, 0x0fc6310b, 0x57e0f83d, 0x8cc26e74, 0x11a525a2, 0x946934c7,
    0x7cd888f0, 0x8f9d8604, 0x4f86e73b, 0x04520316, 0xdeeea20c, 0xf1def496, 0x67687288,
    0xf540c5b2, 0x22401484, 0x3478658a, 0xc2385746, 0x01979c2c, 0x5dad73c8, 0x0321f58b,
    0xf0fedbee, 0x92826ddf, 0x284bec73, 0x5b1a1975, 0x03df1e11, 0x20963e01, 0xa17cf12b,
    0x740d776e, 0xa7a6bf3c, 0x01b5cce4, 0x1118aa76, 0xfc6fac0a, 0xce927e9b, 0x00bf2567,
    0x806f216c, 0xbca69056, 0x795bd3e9, 0xc9dc4557, 0x8929b6c2, 0x789d52ec, 0x3f3fbf40,
    0xb9197368, 0xa38c15b5, 0xc3b44fa8, 0xca8333b0, 0xb7e8d590, 0xbe807feb, 0xbf5f8360,
    0xd99e2f5c, 0x372928e1, 0x7c757c4c, 0x0db5b154, 0xc01ede02, 0x1fc86e78, 0x1f3985be,
    0xb4805c77, 0x00c880fa, 0x974c1b12, 0x35ab0214, 0xb2dc840d, 0x5b00ae37, 0xd313b026,
    0xb260969d, 0x7f4c8879, 0x1734c4d3, 0x49068631, 0xb9f6a021, 0x6b863e6f, 0xcee5debf,
    0x29f8c9fb, 0x53dd6880, 0x72b61223, 0x1f67a9fd, 0x0a0f6993, 0x13e59119, 0x11cca12e,
    0xfe6b6766, 0x16b6effc, 0x97918fc4, 0xc2b8a563, 0x94f2f741, 0x0bfa8c9a, 0xd1537ae8,
    0xc1da349c, 0x873c60ca, 0x95005b85, 0x9b5c080e, 0xbc8abbd9, 0xe1eab1d2, 0x6dac9070,
    0x4ea9ebf1, 0xe0cf30d4, 0x1ef5bd7b, 0xd161043e, 0x5d2fa2e2, 0xff5d3cae, 0x86ed9f87,
    0x2aa1daa1, 0xbd731a34, 0x9e8f4b22, 0xb1c2c67a, 0xc21758c9, 0xa182215d, 0xccb01948,
    0x8d168df7, 0x04238cfe, 0x368c3dbc, 0x0aeadca5, 0xbad21c24, 0x0a71fee5, 0x9fc5d872,
    0x54c152c6, 0xfc329483, 0x6783384a, 0xeddb3e1c, 0x65f90e30, 0x884ad098, 0xce81675a,
    0x4b372f7d, 0x68bf9a39, 0x43445f1e, 0x40f8d8cb, 0x90d5acb6, 0x4cd07282, 0x349eeb06,
    0x0c9d5332, 0x520b24ef, 0x80020447, 0x67976491, 0x2f931ca3, 0xfe9b0535, 0xfcd30220,
    0x61a9e6cc, 0xa487d8d7, 0x3f7c5dd1, 0x7d0127c5, 0x48f51d15, 0x60dea871, 0xc9a91cb7,
    0x58b53bb3, 0x9d5e0b2d, 0x624a78b4, 0x30dbee1b, 0x9bdf22e7, 0x1df5c299, 0x2d5643a7,
    0xf4dd35ff, 0x03ca8fd6, 0x53b47ed8, 0x6f2c19aa, 0xfeb0c1f4, 0x49e54438, 0x2f2577e6,
    0xbf876969, 0x72440ea9, 0xfa0bafb8, 0x74f5b3a0, 0x7dd357cd, 0x89ce1358, 0x6ef2cdda,
    0x1e7767f3, 0xa6be9fdb, 0x4f5f88f8, 0xba994a3a, 0x08ca6b65, 0xe0893818, 0x9e00a16a,
    0xf42bfc8f, 0x9972eedc, 0x749c8b51, 0x32c05f5e, 0xd706805f, 0x6bfbb7cf, 0xd9210a10,
    0x31a1db97, 0x923a9559, 0x37a7a1f6, 0x059f8861, 0xca493e62, 0x65157e81, 0x8f6467dd,
    0xab85ff9f, 0x9331aff2, 0x8616b9f5, 0xedbd5695, 0xee7e29b1, 0x313ac44f, 0xb903112f,
    0x432ef649, 0xdc0a36c0, 0x61cf2bba, 0x81474925, 0xa8b6c7ad, 0xee5931de, 0xb2f8158d,
    0x59fb7409, 0x2e3dfaed, 0x9af25a3f, 0xe1fed4d5,
];

#[derive(Clone)]
struct Family {
    n_gr_codewords: [u32; 8],
    not_gr_cw_len: [u32; 8],
    not_gr_prefix_mask: [u32; 8],
    not_gr_suffix_len: [u32; 8],
    xlat_u2l: Vec<u32>,
    xlat_l2u: Vec<u32>,
}

impl Family {
    fn new() -> Self {
        Self {
            n_gr_codewords: [0; 8],
            not_gr_cw_len: [0; 8],
            not_gr_prefix_mask: [0; 8],
            not_gr_suffix_len: [0; 8],
            xlat_u2l: Vec::new(),
            xlat_l2u: Vec::new(),
        }
    }
}

fn ceil_log_2(mut val: u32) -> u32 {
    if val == 1 {
        return 0;
    }
    let mut result = 1;
    val -= 1;
    while val != 0 {
        val >>= 1;
        if val != 0 {
            result += 1;
        }
    }
    result
}

fn family_init(family: &mut Family, bpc: usize, limit: usize) {
    for l in 0..bpc {
        let mut altprefixlen = (limit - bpc) as u32;
        if altprefixlen > BPPMASK[bpc - l] {
            altprefixlen = BPPMASK[bpc - l];
        }

        let altcodewords = BPPMASK[bpc] + 1 - (altprefixlen << l);
        family.n_gr_codewords[l] = altprefixlen << l;
        family.not_gr_cw_len[l] = altprefixlen + ceil_log_2(altcodewords);
        family.not_gr_prefix_mask[l] = BPPMASK[32 - altprefixlen as usize];
        family.not_gr_suffix_len[l] = ceil_log_2(altcodewords);
    }

    let pixelbitmask = BPPMASK[bpc];
    let pixelbitmaskshr = pixelbitmask >> 1;
    family.xlat_u2l.resize((pixelbitmask + 1) as usize, 0);
    family.xlat_l2u.resize((pixelbitmask + 1) as usize, 0);

    for s in 0..=pixelbitmask {
        family.xlat_u2l[s as usize] = if s <= pixelbitmaskshr {
            s << 1
        } else {
            ((pixelbitmask - s) << 1) + 1
        };
    }

    for s in 0..=pixelbitmask {
        family.xlat_l2u[s as usize] = if s & 0x1 == 1 {
            pixelbitmask - (s >> 1)
        } else {
            s >> 1
        };
    }
}

fn quic_image_bpc(image_type: u32) -> u32 {
    match image_type {
        QUIC_IMAGE_TYPE_GRAY => 8,
        QUIC_IMAGE_TYPE_RGB16 => 5,
        QUIC_IMAGE_TYPE_RGB24 | QUIC_IMAGE_TYPE_RGB32 | QUIC_IMAGE_TYPE_RGBA => 8,
        _ => 0,
    }
}

fn cnt_l_zeroes(bits: u32) -> u32 {
    if bits & 0xff80_0000 != 0 {
        LZEROES[(bits >> 24) as usize] as u32
    } else if bits & 0xffff_8000 != 0 {
        8 + LZEROES[((bits >> 16) & 0xff) as usize] as u32
    } else if bits & 0xffff_ff80 != 0 {
        16 + LZEROES[((bits >> 8) & 0xff) as usize] as u32
    } else {
        24 + LZEROES[(bits & 0xff) as usize] as u32
    }
}

fn golomb_decoding_8bpc(l: usize, bits: u32, family_8bpc: &Family) -> (u32, u32) {
    if (bits as i32) < 0 || bits > family_8bpc.not_gr_prefix_mask[l] {
        let zeroprefix = cnt_l_zeroes(bits);
        let cwlen = zeroprefix + 1 + l as u32;
        let rc = (zeroprefix << l) | ((bits >> (32 - cwlen)) & BPPMASK[l]);
        (cwlen, rc)
    } else {
        let cwlen = family_8bpc.not_gr_cw_len[l];
        let rc = family_8bpc.n_gr_codewords[l]
            + ((bits >> (32 - cwlen)) & BPPMASK[family_8bpc.not_gr_suffix_len[l] as usize]);
        (cwlen, rc)
    }
}

fn golomb_code_len_8bpc(n: u32, l: usize, family_8bpc: &Family) -> u32 {
    if n < family_8bpc.n_gr_codewords[l] {
        (n >> l) + 1 + l as u32
    } else {
        family_8bpc.not_gr_cw_len[l]
    }
}

#[derive(Clone)]
struct QuicModel {
    n_buckets: usize,
    repfirst: u32,
    firstsize: u32,
    repnext: u32,
    mulsize: u32,
    levels: u32,
}

impl QuicModel {
    fn new(bpc: u32) -> Self {
        let levels = 1u32 << bpc;
        let (repfirst, firstsize, repnext, mulsize) = match DEFEVOL {
            1 => (3, 1, 2, 2),
            3 => (1, 1, 1, 2),
            5 => (1, 1, 1, 4),
            _ => (1, 1, 1, 2),
        };

        let mut n_buckets = 0usize;
        let mut repcntr = repfirst + 1;
        let mut bsize = firstsize;
        let mut bend = 0u32;

        loop {
            let bstart = if n_buckets > 0 { bend + 1 } else { 0 };
            repcntr -= 1;
            if repcntr == 0 {
                repcntr = repnext;
                bsize *= mulsize;
            }

            bend = bstart + bsize - 1;
            if bend + bsize >= levels {
                bend = levels - 1;
            }
            n_buckets += 1;
            if bend >= levels - 1 {
                break;
            }
        }

        Self {
            n_buckets,
            repfirst,
            firstsize,
            repnext,
            mulsize,
            levels,
        }
    }
}

#[derive(Clone)]
struct QuicBucket {
    bestcode: usize,
    counters: [u32; 8],
}

impl QuicBucket {
    fn new() -> Self {
        Self {
            bestcode: 0,
            counters: [0; 8],
        }
    }

    fn reset(&mut self, bpp: usize) {
        self.bestcode = bpp;
        self.counters = [0; 8];
    }

    fn update_model_8bpc(&mut self, state: &mut CommonState, curval: u32, bpp: usize, family_8bpc: &Family) {
        let mut bestcode = bpp - 1;
        self.counters[bestcode] = self.counters[bestcode]
            .saturating_add(golomb_code_len_8bpc(curval, bestcode, family_8bpc));
        let mut bestcodelen = self.counters[bestcode];

        for i in (0..=(bpp - 2)).rev() {
            self.counters[i] = self.counters[i].saturating_add(golomb_code_len_8bpc(curval, i, family_8bpc));
            if self.counters[i] < bestcodelen {
                bestcode = i;
                bestcodelen = self.counters[i];
            }
        }

        self.bestcode = bestcode;
        if bestcodelen > state.wm_trigger {
            for i in 0..bpp {
                self.counters[i] >>= 1;
            }
        }
    }
}

#[derive(Clone)]
struct QuicFamilyStat {
    buckets_ptrs: Vec<usize>,
    n_buckets: usize,
}

impl QuicFamilyStat {
    fn new() -> Self {
        Self {
            buckets_ptrs: Vec::new(),
            n_buckets: 0,
        }
    }

    fn fill_model_structures(&mut self, model: &QuicModel) {
        let mut bend = 0u32;
        let mut bnumber = 0usize;
        let mut repcntr = model.repfirst + 1;
        let mut bsize = model.firstsize;

        self.buckets_ptrs.resize(model.levels as usize, 0);

        loop {
            let bstart = if bnumber > 0 { bend + 1 } else { 0 };
            repcntr -= 1;
            if repcntr == 0 {
                repcntr = model.repnext;
                bsize *= model.mulsize;
            }

            bend = bstart + bsize - 1;
            if bend + bsize >= model.levels {
                bend = model.levels - 1;
            }

            for i in bstart..=bend {
                self.buckets_ptrs[i as usize] = bnumber;
            }
            bnumber += 1;

            if bend >= model.levels - 1 {
                break;
            }
        }

        self.n_buckets = bnumber;
    }
}

#[derive(Clone)]
struct CorrelateRow {
    zero: u32,
    row: Vec<u32>,
}

#[derive(Clone)]
struct CommonState {
    waitcnt: usize,
    tabrand_seed: u32,
    wm_trigger: u32,
    wmidx: usize,
    wmileft: usize,
    melcstate: usize,
    melclen: usize,
    melcorder: usize,
}

impl CommonState {
    fn new() -> Self {
        let mut s = Self {
            waitcnt: 0,
            tabrand_seed: 0xff,
            wm_trigger: 0,
            wmidx: 0,
            wmileft: DEFWMINEXT as usize,
            melcstate: 0,
            melclen: 0,
            melcorder: 1,
        };
        s.reset();
        s
    }

    fn set_wm_trigger(&mut self) {
        let wm = self.wmidx.min(10);
        self.wm_trigger = BESTTRIGTAB[DEFEVOL / 2][wm];
    }

    fn reset(&mut self) {
        self.waitcnt = 0;
        self.tabrand_seed = 0x0ff;
        self.wmidx = 0;
        self.wmileft = DEFWMINEXT as usize;
        self.set_wm_trigger();
        self.melcstate = 0;
        self.melclen = J[0] as usize;
        self.melcorder = 1 << self.melclen;
    }

    fn tabrand(&mut self) -> u32 {
        self.tabrand_seed = self.tabrand_seed.wrapping_add(1);
        TABRAND_CHAOS[(self.tabrand_seed & 0xff) as usize]
    }
}

#[derive(Clone)]
struct QuicChannel {
    state: CommonState,
    family_stat_8bpc: QuicFamilyStat,
    family_stat_5bpc: QuicFamilyStat,
    correlate_row: CorrelateRow,
    buckets_ptrs: Vec<usize>,
    buckets_buf: Vec<QuicBucket>,
}

impl QuicChannel {
    fn new(model_8bpc: &QuicModel, model_5bpc: &QuicModel) -> Self {
        let mut family_stat_8bpc = QuicFamilyStat::new();
        family_stat_8bpc.fill_model_structures(model_8bpc);

        let mut family_stat_5bpc = QuicFamilyStat::new();
        family_stat_5bpc.fill_model_structures(model_5bpc);

        Self {
            state: CommonState::new(),
            family_stat_8bpc,
            family_stat_5bpc,
            correlate_row: CorrelateRow {
                zero: 0,
                row: Vec::new(),
            },
            buckets_ptrs: Vec::new(),
            buckets_buf: Vec::new(),
        }
    }

    fn reset(&mut self, bpc: u32, width: usize) -> bool {
        self.correlate_row.zero = 0;
        self.correlate_row.row.clear();
        self.correlate_row.row.resize(width, 0);

        match bpc {
            8 => {
                self.buckets_ptrs = self.family_stat_8bpc.buckets_ptrs.clone();
                self.buckets_buf = vec![QuicBucket::new(); self.family_stat_8bpc.n_buckets];
                for b in &mut self.buckets_buf {
                    b.reset(7);
                }
            }
            5 => {
                self.buckets_ptrs = self.family_stat_5bpc.buckets_ptrs.clone();
                self.buckets_buf = vec![QuicBucket::new(); self.family_stat_5bpc.n_buckets];
                for b in &mut self.buckets_buf {
                    b.reset(4);
                }
            }
            _ => return false,
        }

        self.state.reset();
        true
    }
}

struct QuicDecoder {
    image_type: u32,
    width: usize,
    height: usize,
    io_idx: usize,
    io_available_bits: u32,
    io_word: u32,
    io_next_word: u32,
    io_now: Vec<u8>,
    io_end: usize,
    rows_completed: usize,
    rgb_state: CommonState,
    channels: [QuicChannel; 4],
    model_8bpc: QuicModel,
    family_8bpc: Family,
    zero_lut: [u8; 256],
}

impl QuicDecoder {
    fn new() -> Self {
        let mut family_8bpc = Family::new();
        family_init(&mut family_8bpc, 8, DEFMAXCLEN);

        let mut _family_5bpc = Family::new();
        family_init(&mut _family_5bpc, 5, DEFMAXCLEN);

        let model_8bpc = QuicModel::new(8);
        let model_5bpc = QuicModel::new(5);

        let mut zero_lut = [0u8; 256];
        let mut j = 1usize;
        let mut k = 1usize;
        let mut l = 8u8;
        for slot in &mut zero_lut {
            *slot = l;
            k -= 1;
            if k == 0 {
                k = j;
                l = l.saturating_sub(1);
                j *= 2;
            }
        }

        Self {
            image_type: QUIC_IMAGE_TYPE_INVALID,
            width: 0,
            height: 0,
            io_idx: 0,
            io_available_bits: 0,
            io_word: 0,
            io_next_word: 0,
            io_now: Vec::new(),
            io_end: 0,
            rows_completed: 0,
            rgb_state: CommonState::new(),
            channels: [
                QuicChannel::new(&model_8bpc, &model_5bpc),
                QuicChannel::new(&model_8bpc, &model_5bpc),
                QuicChannel::new(&model_8bpc, &model_5bpc),
                QuicChannel::new(&model_8bpc, &model_5bpc),
            ],
            model_8bpc,
            family_8bpc,
            zero_lut,
        }
    }

    fn reset(&mut self, io_ptr: &[u8]) {
        self.rgb_state.reset();
        self.io_now.clear();
        self.io_now.extend_from_slice(io_ptr);
        self.io_end = self.io_now.len();
        self.io_idx = 0;
        self.rows_completed = 0;
    }

    fn read_io_word(&mut self) -> bool {
        if self.io_idx + 4 > self.io_end {
            return false;
        }
        self.io_next_word = (self.io_now[self.io_idx] as u32)
            | ((self.io_now[self.io_idx + 1] as u32) << 8)
            | ((self.io_now[self.io_idx + 2] as u32) << 16)
            | ((self.io_now[self.io_idx + 3] as u32) << 24);
        self.io_idx += 4;
        true
    }

    fn decode_eatbits(&mut self, len: u32) -> bool {
        self.io_word <<= len;
        let delta = self.io_available_bits as i32 - len as i32;
        if delta >= 0 {
            self.io_available_bits = delta as u32;
            self.io_word |= self.io_next_word >> self.io_available_bits;
        } else {
            let delta = (-delta) as u32;
            self.io_word |= self.io_next_word << delta;
            if !self.read_io_word() {
                return false;
            }
            self.io_available_bits = 32 - delta;
            self.io_word |= self.io_next_word >> self.io_available_bits;
        }
        true
    }

    fn decode_eat32bits(&mut self) -> bool {
        self.decode_eatbits(16) && self.decode_eatbits(16)
    }

    fn reset_channels(&mut self, bpc: u32) -> bool {
        for c in 0..4 {
            if !self.channels[c].reset(bpc, self.width) {
                return false;
            }
        }
        true
    }

    fn quic_decode_begin(&mut self, io_ptr: &[u8]) -> bool {
        self.reset(io_ptr);
        if !self.read_io_word() {
            return false;
        }
        self.io_word = self.io_next_word;
        self.io_available_bits = 0;

        let magic = self.io_word;
        if !self.decode_eat32bits() {
            return false;
        }
        if magic != 0x4349_5551 {
            return false;
        }

        let version = self.io_word;
        if !self.decode_eat32bits() {
            return false;
        }
        if version != 0 {
            return false;
        }

        self.image_type = self.io_word;
        if !self.decode_eat32bits() {
            return false;
        }

        self.width = self.io_word as usize;
        if !self.decode_eat32bits() {
            return false;
        }

        self.height = self.io_word as usize;
        if !self.decode_eat32bits() {
            return false;
        }

        let bpc = quic_image_bpc(self.image_type);
        self.reset_channels(bpc)
    }

    fn decode_run(&mut self, state: &mut CommonState) -> Option<usize> {
        let mut runlen = 0usize;
        loop {
            let x = (!(self.io_word >> 24)) & 0xff;
            let temp = self.zero_lut[x as usize] as usize;

            for _ in 1..=temp {
                runlen += state.melcorder;
                if state.melcstate < 32 {
                    state.melcstate += 1;
                    state.melclen = J[state.melcstate] as usize;
                    state.melcorder = 1usize << state.melclen;
                }
            }

            if temp != 8 {
                if !self.decode_eatbits((temp + 1) as u32) {
                    return None;
                }
                break;
            }
            if !self.decode_eatbits(8) {
                return None;
            }
        }

        if state.melclen != 0 {
            runlen += (self.io_word >> (32 - state.melclen as u32)) as usize;
            if !self.decode_eatbits(state.melclen as u32) {
                return None;
            }
        }

        if state.melcstate != 0 {
            state.melcstate -= 1;
            state.melclen = J[state.melcstate] as usize;
            state.melcorder = 1usize << state.melclen;
        }
        Some(runlen)
    }

    fn decode_run_rgb(&mut self) -> Option<usize> {
        let mut state = self.rgb_state.clone();
        let run = self.decode_run(&mut state)?;
        self.rgb_state = state;
        Some(run)
    }

    fn decode_run_channel(&mut self, channel_index: usize) -> Option<usize> {
        let mut state = self.channels[channel_index].state.clone();
        let run = self.decode_run(&mut state)?;
        self.channels[channel_index].state = state;
        Some(run)
    }

    fn quic_rgb32_uncompress_row0_seg(
        &mut self,
        mut i: usize,
        cur_row: &mut [u8],
        end: usize,
        waitmask: u32,
        bpc: usize,
        bpc_mask: u8,
    ) -> bool {
        let n_channels = 3usize;
        let mut stopidx;

        if i == 0 {
            cur_row[RGB32_PIXEL_PAD] = 0;
            for c in 0..n_channels {
                let bestcode = {
                    let ch = &self.channels[c];
                    let bidx = ch.buckets_ptrs[ch.correlate_row.zero as usize];
                    ch.buckets_buf[bidx].bestcode
                };
                let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                self.channels[c].correlate_row.row[0] = rc;
                cur_row[2 - c] = self.family_8bpc.xlat_l2u[rc as usize] as u8;
                if !self.decode_eatbits(cwlen) {
                    return false;
                }
            }

            if self.rgb_state.waitcnt > 0 {
                self.rgb_state.waitcnt -= 1;
            } else {
                self.rgb_state.waitcnt = (self.rgb_state.tabrand() & waitmask) as usize;
                for c in 0..n_channels {
                    let rc0 = self.channels[c].correlate_row.row[0];
                    let z = self.channels[c].correlate_row.zero as usize;
                    let bidx = self.channels[c].buckets_ptrs[z];
                    self.channels[c].buckets_buf[bidx]
                        .update_model_8bpc(&mut self.rgb_state, rc0, bpc, &self.family_8bpc);
                }
            }
            i += 1;
            stopidx = i + self.rgb_state.waitcnt;
        } else {
            stopidx = i + self.rgb_state.waitcnt;
        }

        while stopidx < end {
            while i <= stopidx {
                let pixel = i * RGB32_PIXEL_SIZE;
                cur_row[pixel + RGB32_PIXEL_PAD] = 0;

                for c in 0..n_channels {
                    let prev_idx = self.channels[c].correlate_row.row[i - 1] as usize;
                    let bidx = self.channels[c].buckets_ptrs[prev_idx];
                    let bestcode = self.channels[c].buckets_buf[bidx].bestcode;
                    let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                    self.channels[c].correlate_row.row[i] = rc;
                    let left = cur_row[(i - 1) * RGB32_PIXEL_SIZE + (2 - c)];
                    cur_row[pixel + (2 - c)] = self.family_8bpc.xlat_l2u[rc as usize]
                        .wrapping_add(left as u32) as u8
                        & bpc_mask;
                    if !self.decode_eatbits(cwlen) {
                        return false;
                    }
                }
                i += 1;
            }

            for c in 0..n_channels {
                let model_key = self.channels[c].correlate_row.row[stopidx - 1] as usize;
                let bidx = self.channels[c].buckets_ptrs[model_key];
                let val = self.channels[c].correlate_row.row[stopidx];
                self.channels[c].buckets_buf[bidx]
                    .update_model_8bpc(&mut self.rgb_state, val, bpc, &self.family_8bpc);
            }
            stopidx = i + ((self.rgb_state.tabrand() & waitmask) as usize);
        }

        while i < end {
            let pixel = i * RGB32_PIXEL_SIZE;
            cur_row[pixel + RGB32_PIXEL_PAD] = 0;

            for c in 0..n_channels {
                let prev_idx = self.channels[c].correlate_row.row[i - 1] as usize;
                let bidx = self.channels[c].buckets_ptrs[prev_idx];
                let bestcode = self.channels[c].buckets_buf[bidx].bestcode;
                let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                self.channels[c].correlate_row.row[i] = rc;
                let left = cur_row[(i - 1) * RGB32_PIXEL_SIZE + (2 - c)];
                cur_row[pixel + (2 - c)] = self.family_8bpc.xlat_l2u[rc as usize]
                    .wrapping_add(left as u32) as u8
                    & bpc_mask;
                if !self.decode_eatbits(cwlen) {
                    return false;
                }
            }
            i += 1;
        }
        self.rgb_state.waitcnt = stopidx.saturating_sub(end);
        true
    }

    fn quic_rgb32_uncompress_row0(&mut self, cur_row: &mut [u8]) -> bool {
        let mut pos = 0usize;
        let mut width = self.width;

        while DEFWMIMAX as usize > self.rgb_state.wmidx && self.rgb_state.wmileft <= width {
            if self.rgb_state.wmileft > 0 {
                if !self.quic_rgb32_uncompress_row0_seg(
                    pos,
                    cur_row,
                    pos + self.rgb_state.wmileft,
                    BPPMASK[self.rgb_state.wmidx],
                    8,
                    0xff,
                ) {
                    return false;
                }
                pos += self.rgb_state.wmileft;
                width -= self.rgb_state.wmileft;
            }
            self.rgb_state.wmidx += 1;
            self.rgb_state.set_wm_trigger();
            self.rgb_state.wmileft = DEFWMINEXT as usize;
        }

        if width > 0 {
            if !self.quic_rgb32_uncompress_row0_seg(
                pos,
                cur_row,
                pos + width,
                BPPMASK[self.rgb_state.wmidx],
                8,
                0xff,
            ) {
                return false;
            }
            if DEFWMIMAX as usize > self.rgb_state.wmidx {
                self.rgb_state.wmileft -= width;
            }
        }
        true
    }

    fn quic_rgb32_uncompress_row_seg(
        &mut self,
        prev_row: &[u8],
        cur_row: &mut [u8],
        mut i: usize,
        end: usize,
        bpc: usize,
        bpc_mask: u8,
    ) -> bool {
        let n_channels = 3usize;
        let waitmask = BPPMASK[self.rgb_state.wmidx];
        let mut run_index = 0usize;
        let mut stopidx;

        if i == 0 {
            cur_row[RGB32_PIXEL_PAD] = 0;
            for c in 0..n_channels {
                let bestcode = {
                    let ch = &self.channels[c];
                    let bidx = ch.buckets_ptrs[ch.correlate_row.zero as usize];
                    ch.buckets_buf[bidx].bestcode
                };
                let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                self.channels[c].correlate_row.row[0] = rc;
                let p = prev_row[2 - c] as u32;
                cur_row[2 - c] = (self.family_8bpc.xlat_l2u[rc as usize] + p) as u8 & bpc_mask;
                if !self.decode_eatbits(cwlen) {
                    return false;
                }
            }

            if self.rgb_state.waitcnt > 0 {
                self.rgb_state.waitcnt -= 1;
            } else {
                self.rgb_state.waitcnt = (self.rgb_state.tabrand() & waitmask) as usize;
                for c in 0..n_channels {
                    let z = self.channels[c].correlate_row.zero as usize;
                    let bidx = self.channels[c].buckets_ptrs[z];
                    let v = self.channels[c].correlate_row.row[0];
                    self.channels[c].buckets_buf[bidx]
                        .update_model_8bpc(&mut self.rgb_state, v, bpc, &self.family_8bpc);
                }
            }
            i += 1;
            stopidx = i + self.rgb_state.waitcnt;
        } else {
            stopidx = i + self.rgb_state.waitcnt;
        }

        loop {
            let mut rc_break = false;

            while stopidx < end && !rc_break {
                while i <= stopidx && !rc_break {
                    let pixel = i * RGB32_PIXEL_SIZE;
                    let pixelm1 = (i - 1) * RGB32_PIXEL_SIZE;
                    let pixelm2 = (i.saturating_sub(2)) * RGB32_PIXEL_SIZE;

                    if prev_row[pixelm1 + RGB32_PIXEL_R] == prev_row[pixel + RGB32_PIXEL_R]
                        && prev_row[pixelm1 + RGB32_PIXEL_G] == prev_row[pixel + RGB32_PIXEL_G]
                        && prev_row[pixelm1 + RGB32_PIXEL_B] == prev_row[pixel + RGB32_PIXEL_B]
                        && run_index != i
                        && i > 2
                        && cur_row[pixelm1 + RGB32_PIXEL_R] == cur_row[pixelm2 + RGB32_PIXEL_R]
                        && cur_row[pixelm1 + RGB32_PIXEL_G] == cur_row[pixelm2 + RGB32_PIXEL_G]
                        && cur_row[pixelm1 + RGB32_PIXEL_B] == cur_row[pixelm2 + RGB32_PIXEL_B]
                    {
                        self.rgb_state.waitcnt = stopidx - i;
                        run_index = i;
                        let run = match self.decode_run_rgb() {
                            Some(v) => v,
                            None => return false,
                        };
                        let run_end = i + run;

                        while i < run_end {
                            let p = i * RGB32_PIXEL_SIZE;
                            let pm1 = (i - 1) * RGB32_PIXEL_SIZE;
                            cur_row[p + RGB32_PIXEL_PAD] = 0;
                            cur_row[p + RGB32_PIXEL_R] = cur_row[pm1 + RGB32_PIXEL_R];
                            cur_row[p + RGB32_PIXEL_G] = cur_row[pm1 + RGB32_PIXEL_G];
                            cur_row[p + RGB32_PIXEL_B] = cur_row[pm1 + RGB32_PIXEL_B];
                            i += 1;
                        }

                        if i == end {
                            return true;
                        }

                        stopidx = i + self.rgb_state.waitcnt;
                        rc_break = true;
                        break;
                    }

                    cur_row[pixel + RGB32_PIXEL_PAD] = 0;
                    for c in 0..n_channels {
                        let prev_idx = self.channels[c].correlate_row.row[i - 1] as usize;
                        let bidx = self.channels[c].buckets_ptrs[prev_idx];
                        let bestcode = self.channels[c].buckets_buf[bidx].bestcode;
                        let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                        self.channels[c].correlate_row.row[i] = rc;
                        let predicted = ((cur_row[pixelm1 + (2 - c)] as u16 + prev_row[pixel + (2 - c)] as u16) >> 1) as u32;
                        cur_row[pixel + (2 - c)] = (self.family_8bpc.xlat_l2u[rc as usize] + predicted) as u8 & bpc_mask;
                        if !self.decode_eatbits(cwlen) {
                            return false;
                        }
                    }
                    i += 1;
                }
                if rc_break {
                    break;
                }

                for c in 0..n_channels {
                    let key = self.channels[c].correlate_row.row[stopidx - 1] as usize;
                    let bidx = self.channels[c].buckets_ptrs[key];
                    let value = self.channels[c].correlate_row.row[stopidx];
                    self.channels[c].buckets_buf[bidx]
                        .update_model_8bpc(&mut self.rgb_state, value, bpc, &self.family_8bpc);
                }
                stopidx = i + ((self.rgb_state.tabrand() & waitmask) as usize);
            }

            while i < end && !rc_break {
                let pixel = i * RGB32_PIXEL_SIZE;
                let pixelm1 = (i - 1) * RGB32_PIXEL_SIZE;
                let pixelm2 = (i.saturating_sub(2)) * RGB32_PIXEL_SIZE;

                if prev_row[pixelm1 + RGB32_PIXEL_R] == prev_row[pixel + RGB32_PIXEL_R]
                    && prev_row[pixelm1 + RGB32_PIXEL_G] == prev_row[pixel + RGB32_PIXEL_G]
                    && prev_row[pixelm1 + RGB32_PIXEL_B] == prev_row[pixel + RGB32_PIXEL_B]
                    && run_index != i
                    && i > 2
                    && cur_row[pixelm1 + RGB32_PIXEL_R] == cur_row[pixelm2 + RGB32_PIXEL_R]
                    && cur_row[pixelm1 + RGB32_PIXEL_G] == cur_row[pixelm2 + RGB32_PIXEL_G]
                    && cur_row[pixelm1 + RGB32_PIXEL_B] == cur_row[pixelm2 + RGB32_PIXEL_B]
                {
                    self.rgb_state.waitcnt = stopidx.saturating_sub(i);
                    run_index = i;
                    let run = match self.decode_run_rgb() {
                        Some(v) => v,
                        None => return false,
                    };
                    let run_end = i + run;

                    while i < run_end {
                        let p = i * RGB32_PIXEL_SIZE;
                        let pm1 = (i - 1) * RGB32_PIXEL_SIZE;
                        cur_row[p + RGB32_PIXEL_PAD] = 0;
                        cur_row[p + RGB32_PIXEL_R] = cur_row[pm1 + RGB32_PIXEL_R];
                        cur_row[p + RGB32_PIXEL_G] = cur_row[pm1 + RGB32_PIXEL_G];
                        cur_row[p + RGB32_PIXEL_B] = cur_row[pm1 + RGB32_PIXEL_B];
                        i += 1;
                    }

                    if i == end {
                        return true;
                    }

                    stopidx = i + self.rgb_state.waitcnt;
                    rc_break = true;
                    break;
                }

                cur_row[pixel + RGB32_PIXEL_PAD] = 0;
                for c in 0..n_channels {
                    let prev_idx = self.channels[c].correlate_row.row[i - 1] as usize;
                    let bidx = self.channels[c].buckets_ptrs[prev_idx];
                    let bestcode = self.channels[c].buckets_buf[bidx].bestcode;
                    let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                    self.channels[c].correlate_row.row[i] = rc;
                    let predicted = ((cur_row[pixelm1 + (2 - c)] as u16 + prev_row[pixel + (2 - c)] as u16) >> 1) as u32;
                    cur_row[pixel + (2 - c)] = (self.family_8bpc.xlat_l2u[rc as usize] + predicted) as u8 & bpc_mask;
                    if !self.decode_eatbits(cwlen) {
                        return false;
                    }
                }
                i += 1;
            }

            if !rc_break {
                self.rgb_state.waitcnt = stopidx.saturating_sub(end);
                return true;
            }
        }
    }

    fn quic_rgb32_uncompress_row(&mut self, prev_row: &[u8], cur_row: &mut [u8]) -> bool {
        let mut pos = 0usize;
        let mut width = self.width;

        while DEFWMIMAX as usize > self.rgb_state.wmidx && self.rgb_state.wmileft <= width {
            if self.rgb_state.wmileft > 0 {
                if !self.quic_rgb32_uncompress_row_seg(
                    prev_row,
                    cur_row,
                    pos,
                    pos + self.rgb_state.wmileft,
                    8,
                    0xff,
                ) {
                    return false;
                }
                pos += self.rgb_state.wmileft;
                width -= self.rgb_state.wmileft;
            }
            self.rgb_state.wmidx += 1;
            self.rgb_state.set_wm_trigger();
            self.rgb_state.wmileft = DEFWMINEXT as usize;
        }

        if width > 0 {
            if !self.quic_rgb32_uncompress_row_seg(prev_row, cur_row, pos, pos + width, 8, 0xff) {
                return false;
            }
            if DEFWMIMAX as usize > self.rgb_state.wmidx {
                self.rgb_state.wmileft -= width;
            }
        }
        true
    }

    fn quic_four_uncompress_row0_seg(
        &mut self,
        channel_index: usize,
        mut i: usize,
        cur_row: &mut [u8],
        end: usize,
        waitmask: u32,
        bpc: usize,
        bpc_mask: u8,
    ) -> bool {
        let mut stopidx;

        if i == 0 {
            let bestcode = {
                let ch = &self.channels[channel_index];
                let bidx = ch.buckets_ptrs[ch.correlate_row.zero as usize];
                ch.buckets_buf[bidx].bestcode
            };
            let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
            self.channels[channel_index].correlate_row.row[0] = rc;
            cur_row[RGB32_PIXEL_PAD] = self.family_8bpc.xlat_l2u[rc as usize] as u8;
            if !self.decode_eatbits(cwlen) {
                return false;
            }

            if self.channels[channel_index].state.waitcnt > 0 {
                self.channels[channel_index].state.waitcnt -= 1;
            } else {
                let wait = {
                    let ch = &mut self.channels[channel_index];
                    (ch.state.tabrand() & waitmask) as usize
                };
                self.channels[channel_index].state.waitcnt = wait;

                let z = self.channels[channel_index].correlate_row.zero as usize;
                let bidx = self.channels[channel_index].buckets_ptrs[z];
                let val = self.channels[channel_index].correlate_row.row[0];
                {
                    let ch = &mut self.channels[channel_index];
                    let (state, buckets) = (&mut ch.state, &mut ch.buckets_buf);
                    buckets[bidx].update_model_8bpc(state, val, bpc, &self.family_8bpc);
                }
            }

            i += 1;
            stopidx = i + self.channels[channel_index].state.waitcnt;
        } else {
            stopidx = i + self.channels[channel_index].state.waitcnt;
        }

        while stopidx < end {
            let mut last_bucket = 0usize;
            while i <= stopidx {
                let key = self.channels[channel_index].correlate_row.row[i - 1] as usize;
                let bidx = self.channels[channel_index].buckets_ptrs[key];
                last_bucket = bidx;

                let bestcode = self.channels[channel_index].buckets_buf[bidx].bestcode;
                let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                self.channels[channel_index].correlate_row.row[i] = rc;
                let left = cur_row[(i - 1) * RGB32_PIXEL_SIZE + RGB32_PIXEL_PAD] as u32;
                cur_row[i * RGB32_PIXEL_SIZE + RGB32_PIXEL_PAD] =
                    (self.family_8bpc.xlat_l2u[rc as usize] + left) as u8 & bpc_mask;
                if !self.decode_eatbits(cwlen) {
                    return false;
                }
                i += 1;
            }

            let value = self.channels[channel_index].correlate_row.row[stopidx];
            {
                let ch = &mut self.channels[channel_index];
                let (state, buckets) = (&mut ch.state, &mut ch.buckets_buf);
                buckets[last_bucket].update_model_8bpc(state, value, bpc, &self.family_8bpc);
            }

            let rand = {
                let ch = &mut self.channels[channel_index];
                (ch.state.tabrand() & waitmask) as usize
            };
            stopidx = i + rand;
        }

        while i < end {
            let key = self.channels[channel_index].correlate_row.row[i - 1] as usize;
            let bidx = self.channels[channel_index].buckets_ptrs[key];
            let bestcode = self.channels[channel_index].buckets_buf[bidx].bestcode;
            let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
            self.channels[channel_index].correlate_row.row[i] = rc;
            let left = cur_row[(i - 1) * RGB32_PIXEL_SIZE + RGB32_PIXEL_PAD] as u32;
            cur_row[i * RGB32_PIXEL_SIZE + RGB32_PIXEL_PAD] =
                (self.family_8bpc.xlat_l2u[rc as usize] + left) as u8 & bpc_mask;
            if !self.decode_eatbits(cwlen) {
                return false;
            }
            i += 1;
        }

        self.channels[channel_index].state.waitcnt = stopidx.saturating_sub(end);
        true
    }

    fn quic_four_uncompress_row0(&mut self, channel_index: usize, cur_row: &mut [u8]) -> bool {
        let mut pos = 0usize;
        let mut width = self.width;

        while DEFWMIMAX as usize > self.channels[channel_index].state.wmidx
            && self.channels[channel_index].state.wmileft <= width
        {
            if self.channels[channel_index].state.wmileft > 0 {
                let segment = self.channels[channel_index].state.wmileft;
                let waitmask = BPPMASK[self.channels[channel_index].state.wmidx];
                if !self.quic_four_uncompress_row0_seg(channel_index, pos, cur_row, pos + segment, waitmask, 8, 0xff) {
                    return false;
                }
                pos += segment;
                width -= segment;
            }

            self.channels[channel_index].state.wmidx += 1;
            self.channels[channel_index].state.set_wm_trigger();
            self.channels[channel_index].state.wmileft = DEFWMINEXT as usize;
        }

        if width > 0 {
            let waitmask = BPPMASK[self.channels[channel_index].state.wmidx];
            if !self.quic_four_uncompress_row0_seg(channel_index, pos, cur_row, pos + width, waitmask, 8, 0xff) {
                return false;
            }
            if DEFWMIMAX as usize > self.channels[channel_index].state.wmidx {
                self.channels[channel_index].state.wmileft -= width;
            }
        }
        true
    }

    fn quic_four_uncompress_row_seg(
        &mut self,
        channel_index: usize,
        prev_row: &[u8],
        cur_row: &mut [u8],
        mut i: usize,
        end: usize,
        bpc: usize,
        bpc_mask: u8,
    ) -> bool {
        let waitmask = BPPMASK[self.channels[channel_index].state.wmidx];
        let mut run_index = 0usize;
        let mut stopidx;

        if i == 0 {
            let bestcode = {
                let ch = &self.channels[channel_index];
                let bidx = ch.buckets_ptrs[ch.correlate_row.zero as usize];
                ch.buckets_buf[bidx].bestcode
            };
            let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
            self.channels[channel_index].correlate_row.row[0] = rc;
            cur_row[RGB32_PIXEL_PAD] =
                (self.family_8bpc.xlat_l2u[rc as usize] + prev_row[RGB32_PIXEL_PAD] as u32) as u8
                    & bpc_mask;
            if !self.decode_eatbits(cwlen) {
                return false;
            }

            if self.channels[channel_index].state.waitcnt > 0 {
                self.channels[channel_index].state.waitcnt -= 1;
            } else {
                let wait = {
                    let ch = &mut self.channels[channel_index];
                    (ch.state.tabrand() & waitmask) as usize
                };
                self.channels[channel_index].state.waitcnt = wait;
                let z = self.channels[channel_index].correlate_row.zero as usize;
                let bidx = self.channels[channel_index].buckets_ptrs[z];
                let value = self.channels[channel_index].correlate_row.row[0];
                {
                    let ch = &mut self.channels[channel_index];
                    let (state, buckets) = (&mut ch.state, &mut ch.buckets_buf);
                    buckets[bidx].update_model_8bpc(state, value, bpc, &self.family_8bpc);
                }
            }

            i += 1;
            stopidx = i + self.channels[channel_index].state.waitcnt;
        } else {
            stopidx = i + self.channels[channel_index].state.waitcnt;
        }

        loop {
            let mut rc_break = false;

            while stopidx < end && !rc_break {
                let mut last_bucket = 0usize;
                while i <= stopidx && !rc_break {
                    let pixel = i * RGB32_PIXEL_SIZE;
                    let pixelm1 = (i - 1) * RGB32_PIXEL_SIZE;
                    let pixelm2 = (i.saturating_sub(2)) * RGB32_PIXEL_SIZE;

                    if prev_row[pixelm1 + RGB32_PIXEL_PAD] == prev_row[pixel + RGB32_PIXEL_PAD]
                        && run_index != i
                        && i > 2
                        && cur_row[pixelm1 + RGB32_PIXEL_PAD] == cur_row[pixelm2 + RGB32_PIXEL_PAD]
                    {
                        self.channels[channel_index].state.waitcnt = stopidx - i;
                        run_index = i;
                        let run = match self.decode_run_channel(channel_index) {
                            Some(v) => v,
                            None => return false,
                        };
                        let run_end = i + run;
                        while i < run_end {
                            let p = i * RGB32_PIXEL_SIZE;
                            let pm1 = (i - 1) * RGB32_PIXEL_SIZE;
                            cur_row[p + RGB32_PIXEL_PAD] = cur_row[pm1 + RGB32_PIXEL_PAD];
                            i += 1;
                        }

                        if i == end {
                            return true;
                        }
                        stopidx = i + self.channels[channel_index].state.waitcnt;
                        rc_break = true;
                        break;
                    }

                    let key = self.channels[channel_index].correlate_row.row[i - 1] as usize;
                    let bidx = self.channels[channel_index].buckets_ptrs[key];
                    last_bucket = bidx;
                    let bestcode = self.channels[channel_index].buckets_buf[bidx].bestcode;
                    let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                    self.channels[channel_index].correlate_row.row[i] = rc;
                    let predicted =
                        ((cur_row[pixelm1 + RGB32_PIXEL_PAD] as u16 + prev_row[pixel + RGB32_PIXEL_PAD] as u16)
                            >> 1) as u32;
                    cur_row[pixel + RGB32_PIXEL_PAD] =
                        (self.family_8bpc.xlat_l2u[rc as usize] + predicted) as u8 & bpc_mask;
                    if !self.decode_eatbits(cwlen) {
                        return false;
                    }
                    i += 1;
                }

                if rc_break {
                    break;
                }

                let value = self.channels[channel_index].correlate_row.row[stopidx];
                {
                    let ch = &mut self.channels[channel_index];
                    let (state, buckets) = (&mut ch.state, &mut ch.buckets_buf);
                    buckets[last_bucket].update_model_8bpc(state, value, bpc, &self.family_8bpc);
                }

                let rand = {
                    let ch = &mut self.channels[channel_index];
                    (ch.state.tabrand() & waitmask) as usize
                };
                stopidx = i + rand;
            }

            while i < end && !rc_break {
                let pixel = i * RGB32_PIXEL_SIZE;
                let pixelm1 = (i - 1) * RGB32_PIXEL_SIZE;
                let pixelm2 = (i.saturating_sub(2)) * RGB32_PIXEL_SIZE;
                if prev_row[pixelm1 + RGB32_PIXEL_PAD] == prev_row[pixel + RGB32_PIXEL_PAD]
                    && run_index != i
                    && i > 2
                    && cur_row[pixelm1 + RGB32_PIXEL_PAD] == cur_row[pixelm2 + RGB32_PIXEL_PAD]
                {
                    self.channels[channel_index].state.waitcnt = stopidx.saturating_sub(i);
                    run_index = i;
                    let run = match self.decode_run_channel(channel_index) {
                        Some(v) => v,
                        None => return false,
                    };
                    let run_end = i + run;
                    while i < run_end {
                        let p = i * RGB32_PIXEL_SIZE;
                        let pm1 = (i - 1) * RGB32_PIXEL_SIZE;
                        cur_row[p + RGB32_PIXEL_PAD] = cur_row[pm1 + RGB32_PIXEL_PAD];
                        i += 1;
                    }
                    if i == end {
                        return true;
                    }
                    stopidx = i + self.channels[channel_index].state.waitcnt;
                    rc_break = true;
                    break;
                }

                let key = self.channels[channel_index].correlate_row.row[i - 1] as usize;
                let bidx = self.channels[channel_index].buckets_ptrs[key];
                let bestcode = self.channels[channel_index].buckets_buf[bidx].bestcode;
                let (cwlen, rc) = golomb_decoding_8bpc(bestcode, self.io_word, &self.family_8bpc);
                self.channels[channel_index].correlate_row.row[i] = rc;
                let predicted =
                    ((cur_row[pixelm1 + RGB32_PIXEL_PAD] as u16 + prev_row[pixel + RGB32_PIXEL_PAD] as u16)
                        >> 1) as u32;
                cur_row[pixel + RGB32_PIXEL_PAD] =
                    (self.family_8bpc.xlat_l2u[rc as usize] + predicted) as u8 & bpc_mask;
                if !self.decode_eatbits(cwlen) {
                    return false;
                }
                i += 1;
            }

            if !rc_break {
                self.channels[channel_index].state.waitcnt = stopidx.saturating_sub(end);
                return true;
            }
        }
    }

    fn quic_four_uncompress_row(
        &mut self,
        channel_index: usize,
        prev_row: &[u8],
        cur_row: &mut [u8],
    ) -> bool {
        let mut pos = 0usize;
        let mut width = self.width;

        while DEFWMIMAX as usize > self.channels[channel_index].state.wmidx
            && self.channels[channel_index].state.wmileft <= width
        {
            if self.channels[channel_index].state.wmileft > 0 {
                let seg = self.channels[channel_index].state.wmileft;
                if !self.quic_four_uncompress_row_seg(channel_index, prev_row, cur_row, pos, pos + seg, 8, 0xff) {
                    return false;
                }
                pos += seg;
                width -= seg;
            }

            self.channels[channel_index].state.wmidx += 1;
            self.channels[channel_index].state.set_wm_trigger();
            self.channels[channel_index].state.wmileft = DEFWMINEXT as usize;
        }

        if width > 0 {
            if !self.quic_four_uncompress_row_seg(channel_index, prev_row, cur_row, pos, pos + width, 8, 0xff) {
                return false;
            }
            if DEFWMIMAX as usize > self.channels[channel_index].state.wmidx {
                self.channels[channel_index].state.wmileft -= width;
            }
        }
        true
    }

    fn quic_decode(&mut self, buf: &mut [u8], stride: usize) -> bool {
        match self.image_type {
            QUIC_IMAGE_TYPE_RGB32 | QUIC_IMAGE_TYPE_RGB24 => {
                self.channels[0].correlate_row.zero = 0;
                self.channels[1].correlate_row.zero = 0;
                self.channels[2].correlate_row.zero = 0;

                if !self.quic_rgb32_uncompress_row0(&mut buf[..stride]) {
                    return false;
                }

                self.rows_completed += 1;
                for row in 1..self.height {
                    let split = row * stride;
                    let (head, tail) = buf.split_at_mut(split);
                    let prev_row = &head[(split - stride)..split];
                    let cur_row = &mut tail[..stride];

                    self.channels[0].correlate_row.zero = self.channels[0].correlate_row.row[0];
                    self.channels[1].correlate_row.zero = self.channels[1].correlate_row.row[0];
                    self.channels[2].correlate_row.zero = self.channels[2].correlate_row.row[0];

                    if !self.quic_rgb32_uncompress_row(prev_row, cur_row) {
                        return false;
                    }
                    self.rows_completed += 1;
                }
                true
            }
            QUIC_IMAGE_TYPE_RGBA => {
                self.channels[0].correlate_row.zero = 0;
                self.channels[1].correlate_row.zero = 0;
                self.channels[2].correlate_row.zero = 0;
                if !self.quic_rgb32_uncompress_row0(&mut buf[..stride]) {
                    return false;
                }

                self.channels[3].correlate_row.zero = 0;
                if !self.quic_four_uncompress_row0(3, &mut buf[..stride]) {
                    return false;
                }

                self.rows_completed += 1;
                for row in 1..self.height {
                    let split = row * stride;
                    let (head, tail) = buf.split_at_mut(split);
                    let prev_row = &head[(split - stride)..split];
                    let cur_row = &mut tail[..stride];

                    self.channels[0].correlate_row.zero = self.channels[0].correlate_row.row[0];
                    self.channels[1].correlate_row.zero = self.channels[1].correlate_row.row[0];
                    self.channels[2].correlate_row.zero = self.channels[2].correlate_row.row[0];
                    if !self.quic_rgb32_uncompress_row(prev_row, cur_row) {
                        return false;
                    }

                    self.channels[3].correlate_row.zero = self.channels[3].correlate_row.row[0];
                    if !self.quic_four_uncompress_row(3, prev_row, cur_row) {
                        return false;
                    }
                    self.rows_completed += 1;
                }
                true
            }
            _ => false,
        }
    }
}

pub fn quic_decode(data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    debug!("quic: decode: data_len={}, expected={}x{}", data.len(), width, height);
    let mut decoder = QuicDecoder::new();
    if !decoder.quic_decode_begin(data) {
        warn!("quic: decode_begin failed");
        return None;
    }
    debug!("quic: header: type={}, size={}x{}", decoder.image_type, decoder.width, decoder.height);

    if decoder.image_type != QUIC_IMAGE_TYPE_RGB32
        && decoder.image_type != QUIC_IMAGE_TYPE_RGB24
        && decoder.image_type != QUIC_IMAGE_TYPE_RGBA
    {
        warn!("quic: unsupported image type: {}", decoder.image_type);
        return None;
    }

    if decoder.width as u32 != width || decoder.height as u32 != height {
        warn!("quic: size mismatch: quic={}x{} vs expected={}x{}", decoder.width, decoder.height, width, height);
        return None;
    }

    let stride = decoder.width.checked_mul(4)?;
    let total = decoder.height.checked_mul(stride)?;
    let mut native = vec![0u8; total];
    if !decoder.quic_decode(&mut native, stride) {
        warn!("quic: decode failed");
        return None;
    }

    let mut rgba = vec![0u8; total];
    for i in (0..total).step_by(4) {
        let b = native[i + 0];
        let g = native[i + 1];
        let r = native[i + 2];
        rgba[i + 0] = r;
        rgba[i + 1] = g;
        rgba[i + 2] = b;
        rgba[i + 3] = if decoder.image_type == QUIC_IMAGE_TYPE_RGBA {
            255u8.wrapping_sub(native[i + 3])
        } else {
            255
        };
    }

    Some(rgba)
}
