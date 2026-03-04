//! ReplayGain 1.0 analyzer over decoded PCM audio.
//!
//! This module implements the classic ReplayGain analysis filter chain
//! (10th-order Yule + 2nd-order Butterworth), 50 ms block histogram, and
//! 95th percentile loudness gain estimation.

use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use ebur128::{EbuR128, Mode as EbuR128Mode};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::config::LoudnessStandard;

const YULE_ORDER: usize = 10;
const BUTTER_ORDER: usize = 2;
const STEPS_PER_DB: f64 = 100.0;
const HISTOGRAM_SLOTS: usize = 12_000;
const RMS_PERCENTILE: f64 = 0.95;
const RMS_WINDOW_TIME_SECONDS: f64 = 0.050;
const PINK_REFERENCE_DB: f64 = 64.54;
const EPSILON_POWER: f64 = 1e-16;
const R128_REFERENCE_LOUDNESS_LUFS: f64 = -18.0;
const R128_MIN_DURATION_SECONDS: f64 = 1e-6;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplayGainAnalysisValues {
    pub(crate) gain_db: f32,
    pub(crate) peak: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct ReplayGainTrackAnalysis {
    histogram: Vec<u32>,
    peak: f32,
}

impl ReplayGainTrackAnalysis {
    pub(crate) fn values(&self) -> Result<ReplayGainAnalysisValues, String> {
        values_from_histogram(&self.histogram, self.peak)
    }
}

#[derive(Clone, Copy)]
struct FilterCoefficients {
    sample_rate_hz: u32,
    yule_a: [f64; YULE_ORDER + 1],
    yule_b: [f64; YULE_ORDER + 1],
    butter_a: [f64; BUTTER_ORDER + 1],
    butter_b: [f64; BUTTER_ORDER + 1],
}

// Canonical ReplayGain filter coefficients used by open-source RG1 analyzers.
#[allow(clippy::excessive_precision)]
const FILTER_COEFFICIENTS: &[FilterCoefficients] = &[
    FilterCoefficients {
        sample_rate_hz: 192_000,
        yule_a: [
            1.0,
            -5.24727318348167,
            10.60821585192244,
            -8.74127665810413,
            -1.33906071371683,
            8.07972882096606,
            -5.46179918950847,
            0.54318070652536,
            0.87450969224280,
            -0.34656083539754,
            0.03034796843589,
        ],
        yule_b: [
            0.01184742123123,
            -0.04631092400086,
            0.06584226961238,
            -0.02165588522478,
            -0.05656260778952,
            0.08607493592760,
            -0.03375544339786,
            -0.04216579932754,
            0.06416711490648,
            -0.03444708260844,
            0.00697275872241,
        ],
        butter_a: [1.0, -1.83317438428207, 0.84010420433555],
        butter_b: [0.91791500137610, -1.83583000275219, 0.91791500137610],
    },
    FilterCoefficients {
        sample_rate_hz: 176_400,
        yule_a: [
            1.0,
            -5.57512782763039,
            12.44291056065794,
            -12.87462799681221,
            3.08554846961576,
            6.62493459880692,
            -7.07662766313248,
            2.51175542736441,
            0.06731510802735,
            -0.24567753819213,
            0.03961404162376,
        ],
        yule_b: [
            0.00268568524529,
            -0.00852379426080,
            0.00852704191347,
            0.00146116310295,
            -0.00950855828762,
            0.00625449515499,
            0.00116183868722,
            -0.00362461417136,
            0.00203961000134,
            -0.00050664587933,
            0.00004327455427,
        ],
        butter_a: [1.0, -1.85916912940418, 0.86491350992834],
        butter_b: [0.93034775234268, -1.86069550468538, 0.93034775234268],
    },
    FilterCoefficients {
        sample_rate_hz: 144_000,
        yule_a: [
            1.0,
            -6.14814623523425,
            15.80002457141566,
            -20.78487587686937,
            11.98848552310315,
            3.36462015062606,
            -10.22419868359470,
            6.65599702146473,
            -1.67141861110485,
            -0.05417956536718,
            0.07374767867406,
        ],
        yule_b: [
            0.00639682359450,
            -0.02556437970955,
            0.04230854400938,
            -0.03722462201267,
            0.01718514827295,
            0.00610592243009,
            -0.03065965747365,
            0.04345745003539,
            -0.03298592681309,
            0.01320937236809,
            -0.00220304127757,
        ],
        butter_a: [1.0, -1.88903307939452, 0.89487434461664],
        butter_b: [0.94597685600279, -1.89195371200558, 0.94597685600279],
    },
    FilterCoefficients {
        sample_rate_hz: 128_000,
        yule_a: [
            1.0,
            -6.14581710839925,
            16.04785903675838,
            -22.19089131407749,
            15.24756471580286,
            -0.52001440400238,
            -8.00488641699940,
            6.60916094768855,
            -2.37856022810923,
            0.33106947986101,
            0.00459820832036,
        ],
        yule_b: [
            0.00553120584305,
            -0.02112620545016,
            0.03549076243117,
            -0.03362498312306,
            0.01425867248183,
            0.01344686928787,
            -0.03392770787836,
            0.03464136459530,
            -0.02039116051549,
            0.00667420794705,
            -0.00093763762995,
        ],
        butter_a: [1.0, -1.91542108074780, 0.91885558323625],
        butter_b: [0.95856916599601, -1.91713833199203, 0.95856916599601],
    },
    FilterCoefficients {
        sample_rate_hz: 112_000,
        yule_a: [
            1.0,
            -6.24932108456288,
            17.42344320538476,
            -27.86819709054896,
            26.79087344681326,
            -13.43711081485123,
            -0.66023612948173,
            6.03658091814935,
            -4.24926577030310,
            1.40829268709186,
            -0.19480852628112,
        ],
        yule_b: [
            0.00528778718259,
            -0.01893240907245,
            0.03185982561867,
            -0.02926260297838,
            0.00715743034072,
            0.01985743355827,
            -0.03222614850941,
            0.02565681978192,
            -0.01210662313473,
            0.00325436284541,
            -0.00044173593001,
        ],
        butter_a: [1.0, -1.91858953033784, 0.92177618768381],
        butter_b: [0.96009142950541, -1.92018285901082, 0.96009142950541],
    },
    FilterCoefficients {
        sample_rate_hz: 96_000,
        yule_a: [
            1.0,
            -5.97808823642008,
            16.21362507964068,
            -25.72923730652599,
            25.40470663139513,
            -14.66166287771134,
            2.81597484359752,
            2.51447125969733,
            -2.23575306985286,
            0.75788151036791,
            -0.10078025199029,
        ],
        yule_b: [
            0.00588138296683,
            -0.01613559730421,
            0.02184798954216,
            -0.01742490405317,
            0.00464635643780,
            0.01117772513205,
            -0.02123865824368,
            0.01959354413350,
            -0.01079720643523,
            0.00352183686289,
            -0.00063124341421,
        ],
        butter_a: [1.0, -1.92783286977036, 0.93034775234268],
        butter_b: [0.96454515552826, -1.92909031105652, 0.96454515552826],
    },
    FilterCoefficients {
        sample_rate_hz: 88_200,
        yule_a: [
            1.0,
            -6.31836451657302,
            18.31351310801799,
            -31.88210014815921,
            36.53792146976740,
            -28.23393036467559,
            14.24725258227189,
            -4.04670980012854,
            0.18865757280515,
            0.25420333563908,
            -0.06012333531065,
        ],
        yule_b: [
            0.02667482047416,
            -0.11377479336097,
            0.23063167910965,
            -0.30726477945593,
            0.33188520686529,
            -0.33862680249063,
            0.31807161531340,
            -0.23730796929880,
            0.12273894790371,
            -0.03840017967282,
            0.00549673387936,
        ],
        butter_a: [1.0, -1.94561023566527, 0.94705070426118],
        butter_b: [0.97316523498161, -1.94633046996323, 0.97316523498161],
    },
    FilterCoefficients {
        sample_rate_hz: 64_000,
        yule_a: [
            1.0,
            -5.73625477092119,
            16.15249794355035,
            -29.68654912464508,
            39.55706155674083,
            -39.82524556246253,
            30.50605345013009,
            -17.43051772821245,
            7.05154573908017,
            -1.80783839720514,
            0.22127840210813,
        ],
        yule_b: [
            0.02613056568174,
            -0.08128786488109,
            0.14937282347325,
            -0.21695711675126,
            0.25010286673402,
            -0.23162283619278,
            0.17424041833052,
            -0.10299599216680,
            0.04258696481981,
            -0.00977952936493,
            0.00105325558889,
        ],
        butter_a: [1.0, -1.95002759149878, 0.95124613669835],
        butter_b: [0.97531843204928, -1.95063686409857, 0.97531843204928],
    },
    FilterCoefficients {
        sample_rate_hz: 56_000,
        yule_a: [
            1.0,
            -4.87377313090032,
            12.03922160140209,
            -20.10151118381395,
            25.10388534415171,
            -24.29065560815903,
            18.27158469090663,
            -10.45249552560593,
            4.30319491872003,
            -1.13716992070185,
            0.14510733527035,
        ],
        yule_b: [
            0.03144914734085,
            -0.06151729206963,
            0.08066788708145,
            -0.09737939921516,
            0.08943210803999,
            -0.06989984672010,
            0.04926972841044,
            -0.03161257848451,
            0.01456837493506,
            -0.00316015108496,
            0.00132807215875,
        ],
        butter_a: [1.0, -1.95835380975398, 0.95920349965459],
        butter_b: [0.97938932735214, -1.95877865470428, 0.97938932735214],
    },
    FilterCoefficients {
        sample_rate_hz: 48_000,
        yule_a: [
            1.0,
            -3.84664617118067,
            7.81501653005538,
            -11.34170355132042,
            13.05504219327545,
            -12.28759895145294,
            9.48293806319790,
            -5.87257861775999,
            2.75465861874613,
            -0.86984376593551,
            0.13919314567432,
        ],
        yule_b: [
            0.03857599435200,
            -0.02160367184185,
            -0.00123395316851,
            -0.00009291677959,
            -0.01655260341619,
            0.02161526843274,
            -0.02074045215285,
            0.00594298065125,
            0.00306428023191,
            0.00012025322027,
            0.00288463683916,
        ],
        butter_a: [1.0, -1.97223372919527, 0.97261396931306],
        butter_b: [0.98621192462708, -1.97242384925416, 0.98621192462708],
    },
    FilterCoefficients {
        sample_rate_hz: 44_100,
        yule_a: [
            1.0,
            -3.47845948550071,
            6.36317777566148,
            -8.54751527471874,
            9.47693607801280,
            -8.81498681370155,
            6.85401540936998,
            -4.39470996079559,
            2.19611684890774,
            -0.75104302451432,
            0.13149317958808,
        ],
        yule_b: [
            0.05418656406430,
            -0.02911007808948,
            -0.00848709379851,
            -0.00851165645469,
            -0.00834990904936,
            0.02245293253339,
            -0.02596338512915,
            0.01624864962975,
            -0.00240879051584,
            0.00674613682247,
            -0.00187763777362,
        ],
        butter_a: [1.0, -1.96977855582618, 0.97022847566350],
        butter_b: [0.98500175787242, -1.97000351574484, 0.98500175787242],
    },
    FilterCoefficients {
        sample_rate_hz: 37_800,
        yule_a: [
            1.0,
            -2.65097999515473,
            3.53734535817992,
            -3.81003448678921,
            3.91291636730132,
            -3.53518605896288,
            2.71356866157873,
            -1.86723311846592,
            1.12075382367659,
            -0.48574086886890,
            0.11330544663849,
        ],
        yule_b: [
            0.08717879977844,
            -0.01000374016172,
            -0.06265852122368,
            -0.01119328800950,
            -0.00114279372960,
            0.02081333954769,
            -0.01603261863207,
            0.01936763028546,
            0.00760044736442,
            -0.00303979112271,
            -0.00075088605788,
        ],
        butter_a: [1.0, -1.96474258269041, 0.96535344991740],
        butter_b: [0.98252400815195, -1.96504801630391, 0.98252400815195],
    },
    FilterCoefficients {
        sample_rate_hz: 32_000,
        yule_a: [
            1.0,
            -2.37898834973084,
            2.84868151156327,
            -2.64577170229825,
            2.23697657451713,
            -1.67148153367602,
            1.00595954808547,
            -0.45953458054983,
            0.16378164858596,
            -0.05032077717131,
            0.02347897407020,
        ],
        yule_b: [
            0.15457299681924,
            -0.09331049056315,
            -0.06247880153653,
            0.02163541888798,
            -0.05588393329856,
            0.04781476674921,
            0.00222312597743,
            0.03174092540049,
            -0.01390589421898,
            0.00651420667831,
            -0.00881362733839,
        ],
        butter_a: [1.0, -1.97887438774988, 0.97968968953528],
        butter_b: [0.98964101933472, -1.97928203866944, 0.98964101933472],
    },
    FilterCoefficients {
        sample_rate_hz: 24_000,
        yule_a: [
            1.0,
            -1.61273165137247,
            1.07977492259970,
            -0.25656257754070,
            -0.16276719120440,
            -0.22638893773906,
            0.39120800788284,
            -0.22138138954925,
            0.04500235387352,
            0.02005851806501,
            0.00302439095741,
        ],
        yule_b: [
            0.30296907319327,
            -0.22613988682123,
            -0.08587323730772,
            0.03282930172664,
            -0.00915702933434,
            -0.02364141202522,
            -0.00584456039913,
            0.06276101321749,
            -0.00000828086748,
            0.00205861885564,
            -0.02950134983287,
        ],
        butter_a: [1.0, -1.98223372919527, 0.98306071056239],
        butter_b: [0.99132360993940, -1.98264721987880, 0.99132360993940],
    },
    FilterCoefficients {
        sample_rate_hz: 22_050,
        yule_a: [
            1.0,
            -1.49858979367799,
            0.87350271418188,
            0.12205022308084,
            -0.80774944671438,
            0.47854794562326,
            -0.12453458140019,
            -0.04067510197014,
            0.08333755284107,
            -0.04237348025746,
            0.02977207319925,
        ],
        yule_b: [
            0.33642304856132,
            -0.25572241425570,
            -0.11828570177555,
            0.11921148675203,
            -0.07834489609479,
            -0.00469977914380,
            -0.00589500224440,
            0.05724228140351,
            0.00832043980773,
            -0.01635381384540,
            -0.01760176568150,
        ],
        butter_a: [1.0, -1.98453370859690, 0.98535649227964],
        butter_b: [0.99247255046129, -1.98494510092258, 0.99247255046129],
    },
    FilterCoefficients {
        sample_rate_hz: 18_900,
        yule_a: [
            1.0,
            -1.29708918404534,
            0.90399339674203,
            -0.29613799017877,
            -0.42326645916207,
            0.37934887402200,
            -0.37919795944938,
            0.23410283284785,
            -0.03892971758879,
            0.00403009552351,
            0.03640166626278,
        ],
        yule_b: [
            0.38524531015142,
            -0.27682212062067,
            -0.09980181488805,
            0.09951486755646,
            -0.08934020156622,
            -0.00322369330199,
            -0.00110329090689,
            0.03784509844682,
            0.01683906213303,
            -0.01147039862572,
            -0.01941767987192,
        ],
        butter_a: [1.0, -1.92950577983524, 0.93190729279793],
        butter_b: [0.96535326815829, -1.93070653631658, 0.96535326815829],
    },
    FilterCoefficients {
        sample_rate_hz: 16_000,
        yule_a: [
            1.0,
            -0.62820619233671,
            0.29661783706366,
            -0.37256372942400,
            0.00213767857124,
            -0.42029820170918,
            0.22199650564824,
            0.00613424350682,
            0.06747620744683,
            0.05784820375801,
            0.03222754072173,
        ],
        yule_b: [
            0.44915256608450,
            -0.14351757464547,
            -0.22784394429749,
            -0.01419140100551,
            0.04078262797139,
            -0.12398163381748,
            0.04097565135648,
            0.10478503600251,
            -0.01863887810927,
            -0.03193428438915,
            0.00541907748707,
        ],
        butter_a: [1.0, -1.98855880571769, 0.98939070721359],
        butter_b: [0.99448737810816, -1.98897475621632, 0.99448737810816],
    },
    FilterCoefficients {
        sample_rate_hz: 12_000,
        yule_a: [
            1.0,
            -1.04800335126349,
            0.29156311971249,
            -0.26806001042947,
            0.00819999645858,
            0.45054734505008,
            -0.33032403314006,
            0.06739368333110,
            -0.04784254229033,
            0.01639907836189,
            0.01807364323573,
        ],
        yule_b: [
            0.56619470757641,
            -0.75464456939302,
            0.16242137742230,
            0.16744243493672,
            -0.18901604199609,
            0.30931782841830,
            -0.27562961986224,
            0.00647310677246,
            0.08647503780351,
            -0.03788984554840,
            -0.00588215443421,
        ],
        butter_a: [1.0, -1.98928677962379, 0.99012785353744],
        butter_b: [0.99485365822456, -1.98970731644912, 0.99485365822456],
    },
    FilterCoefficients {
        sample_rate_hz: 11_025,
        yule_a: [
            1.0,
            -0.51035327095184,
            -0.31863563325245,
            -0.20256413484477,
            0.14728154134330,
            0.38952639978999,
            -0.23313271880868,
            -0.05246019024463,
            -0.02505961724053,
            0.02442357316099,
            0.01818801111503,
        ],
        yule_b: [
            0.58100494960553,
            -0.53174909058578,
            -0.14289799034253,
            0.17520704835522,
            0.02377945217615,
            0.15558449135573,
            -0.25344790059353,
            0.01628462406333,
            0.06920467763959,
            -0.03721611395801,
            -0.00749618797172,
        ],
        butter_a: [1.0, -1.99004745483398, 0.99092768963664],
        butter_b: [0.95856916599601, -1.91713833199203, 0.95856916599601],
    },
    FilterCoefficients {
        sample_rate_hz: 8_000,
        yule_a: [
            1.0,
            -0.25049871956020,
            -0.43193942311114,
            -0.03424681017675,
            -0.04678328784242,
            0.26408300200955,
            0.15113130533216,
            -0.17556493366449,
            -0.18823009262115,
            0.05477720428674,
            0.04704409688120,
        ],
        yule_b: [
            0.53648789255105,
            -0.42163034350696,
            -0.00275953611929,
            0.04267842219415,
            -0.10214864179676,
            0.14590772289388,
            -0.02459864859345,
            -0.11202315195388,
            -0.04060034127000,
            0.04788665548180,
            -0.02217936801134,
        ],
        butter_a: [1.0, -1.99170079625902, 0.99207225036621],
        butter_b: [0.99618138603754, -1.99236277207508, 0.99618138603754],
    },
];

#[derive(Default)]
struct IirYuleState {
    x: [f64; YULE_ORDER + 1],
    y: [f64; YULE_ORDER + 1],
}

impl IirYuleState {
    fn process(&mut self, sample: f64, coeff: &FilterCoefficients) -> f64 {
        for index in (1..=YULE_ORDER).rev() {
            self.x[index] = self.x[index - 1];
            self.y[index] = self.y[index - 1];
        }
        self.x[0] = sample;

        let mut output = coeff.yule_b[0] * self.x[0];
        for index in 1..=YULE_ORDER {
            output += coeff.yule_b[index] * self.x[index] - coeff.yule_a[index] * self.y[index];
        }

        self.y[0] = output;
        output
    }
}

#[derive(Default)]
struct IirButterState {
    x: [f64; BUTTER_ORDER + 1],
    y: [f64; BUTTER_ORDER + 1],
}

impl IirButterState {
    fn process(&mut self, sample: f64, coeff: &FilterCoefficients) -> f64 {
        for index in (1..=BUTTER_ORDER).rev() {
            self.x[index] = self.x[index - 1];
            self.y[index] = self.y[index - 1];
        }
        self.x[0] = sample;

        let mut output = coeff.butter_b[0] * self.x[0];
        for index in 1..=BUTTER_ORDER {
            output += coeff.butter_b[index] * self.x[index] - coeff.butter_a[index] * self.y[index];
        }

        self.y[0] = output;
        output
    }
}

#[derive(Default)]
struct ChannelFilterState {
    yule: IirYuleState,
    butter: IirButterState,
}

struct Rg1PcmAnalyzer {
    coeff: &'static FilterCoefficients,
    channel_states: Vec<ChannelFilterState>,
    channel_count: usize,
    window_size_frames: usize,
    window_sum_squares: f64,
    window_frames: usize,
    histogram: Vec<u32>,
    peak: f32,
}

impl Rg1PcmAnalyzer {
    fn new(sample_rate_hz: u32, channel_count: usize) -> Result<Self, String> {
        if channel_count == 0 {
            return Err("ReplayGain analysis requires at least one channel".to_string());
        }
        let coeff = coefficients_for_sample_rate(sample_rate_hz);
        let window_size_frames =
            ((coeff.sample_rate_hz as f64) * RMS_WINDOW_TIME_SECONDS).ceil() as usize;
        if window_size_frames == 0 {
            return Err("ReplayGain analysis produced an invalid window size".to_string());
        }

        let mut channel_states = Vec::with_capacity(channel_count);
        for _ in 0..channel_count {
            channel_states.push(ChannelFilterState::default());
        }

        Ok(Self {
            coeff,
            channel_states,
            channel_count,
            window_size_frames,
            window_sum_squares: 0.0,
            window_frames: 0,
            histogram: vec![0; HISTOGRAM_SLOTS],
            peak: 0.0,
        })
    }

    fn process_interleaved(&mut self, samples: &[f32]) {
        for frame in samples.chunks_exact(self.channel_count) {
            let mut frame_sum_squares = 0.0f64;
            for (index, sample) in frame.iter().enumerate() {
                self.peak = self.peak.max(sample.abs());
                let scaled_sample = *sample as f64;
                let yule = self.channel_states[index]
                    .yule
                    .process(scaled_sample, self.coeff);
                let filtered = self.channel_states[index].butter.process(yule, self.coeff);
                frame_sum_squares += filtered * filtered;
            }

            self.window_sum_squares += frame_sum_squares;
            self.window_frames = self.window_frames.saturating_add(1);
            if self.window_frames >= self.window_size_frames {
                self.flush_window(self.window_frames);
            }
        }
    }

    fn flush_window(&mut self, frame_count: usize) {
        if frame_count == 0 {
            return;
        }

        // FFmpeg RG1 reference: 10*log10(sum / nb_samples + 1e-16) + 90 - 3.
        let normalized = (self.window_sum_squares / (frame_count as f64)) + EPSILON_POWER;
        let loudness_db = 10.0 * normalized.log10() + 87.0;
        let mut histogram_index = (loudness_db * STEPS_PER_DB).floor() as isize;
        if histogram_index < 0 {
            histogram_index = 0;
        }
        if histogram_index >= HISTOGRAM_SLOTS as isize {
            histogram_index = (HISTOGRAM_SLOTS as isize) - 1;
        }
        self.histogram[histogram_index as usize] =
            self.histogram[histogram_index as usize].saturating_add(1);

        self.window_sum_squares = 0.0;
        self.window_frames = 0;
    }

    fn finish(mut self) -> Result<ReplayGainTrackAnalysis, String> {
        if self.window_frames > 0 {
            self.flush_window(self.window_frames);
        }

        let block_count: u64 = self.histogram.iter().map(|count| *count as u64).sum();
        if block_count == 0 {
            return Err("ReplayGain analysis produced no valid audio blocks".to_string());
        }

        let peak = if self.peak <= 0.0 || !self.peak.is_finite() {
            f32::EPSILON
        } else {
            self.peak
        };

        Ok(ReplayGainTrackAnalysis {
            histogram: self.histogram,
            peak,
        })
    }
}

fn coefficients_for_sample_rate(sample_rate_hz: u32) -> &'static FilterCoefficients {
    if let Some(exact) = FILTER_COEFFICIENTS
        .iter()
        .find(|coeff| coeff.sample_rate_hz == sample_rate_hz)
    {
        return exact;
    }

    FILTER_COEFFICIENTS
        .iter()
        .min_by_key(|coeff| sample_rate_hz.abs_diff(coeff.sample_rate_hz))
        .expect("ReplayGain coefficient table is non-empty")
}

fn gain_from_histogram(histogram: &[u32]) -> Result<f32, String> {
    if histogram.len() != HISTOGRAM_SLOTS {
        return Err("ReplayGain histogram shape is invalid".to_string());
    }

    let total_blocks: u64 = histogram.iter().map(|count| *count as u64).sum();
    if total_blocks == 0 {
        return Err("ReplayGain histogram contains no audio blocks".to_string());
    }

    let mut remaining = ((total_blocks as f64) * (1.0 - RMS_PERCENTILE)).ceil() as u64;
    if remaining == 0 {
        remaining = 1;
    }

    let mut selected_bin = 0usize;
    for index in (0..HISTOGRAM_SLOTS).rev() {
        let count = histogram[index] as u64;
        if count >= remaining {
            selected_bin = index;
            break;
        }
        remaining -= count;
    }

    let gain = (PINK_REFERENCE_DB - ((selected_bin as f64) / STEPS_PER_DB)) as f32;
    Ok(gain.clamp(-24.0, 64.0))
}

fn values_from_histogram(histogram: &[u32], peak: f32) -> Result<ReplayGainAnalysisValues, String> {
    let gain_db = gain_from_histogram(histogram)?;
    if !gain_db.is_finite() {
        return Err("ReplayGain analysis computed a non-finite gain value".to_string());
    }
    if !peak.is_finite() || peak <= 0.0 {
        return Err("ReplayGain analysis computed an invalid peak value".to_string());
    }

    Ok(ReplayGainAnalysisValues { gain_db, peak })
}

fn combine_track_analyses<'a, I>(tracks: I) -> Result<ReplayGainAnalysisValues, String>
where
    I: IntoIterator<Item = &'a ReplayGainTrackAnalysis>,
{
    let mut histogram = vec![0u32; HISTOGRAM_SLOTS];
    let mut peak = 0.0f32;
    let mut track_count = 0usize;

    for track in tracks {
        track_count = track_count.saturating_add(1);
        peak = peak.max(track.peak);
        for (index, count) in track.histogram.iter().enumerate() {
            histogram[index] = histogram[index].saturating_add(*count);
        }
    }

    if track_count == 0 {
        return Err("Cannot compute album ReplayGain for an empty track set".to_string());
    }

    values_from_histogram(&histogram, peak.max(f32::EPSILON))
}

pub(crate) fn analyze_track(path: &Path) -> Result<ReplayGainTrackAnalysis, String> {
    let input = File::open(path).map_err(|error| {
        format!(
            "ReplayGain analysis failed opening {}: {}",
            path.display(),
            error
        )
    })?;
    let mss = MediaSourceStream::new(Box::new(input), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }

    let probe = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| {
            format!(
                "ReplayGain analysis failed probing {}: {}",
                path.display(),
                error
            )
        })?;
    let mut format_reader = probe.format;
    let default_track = format_reader.default_track().ok_or_else(|| {
        format!(
            "ReplayGain analysis found no audio track in {}",
            path.display()
        )
    })?;
    let track_id = default_track.id;
    let codec_params = default_track.codec_params.clone();
    let mut analyzer: Option<Rg1PcmAnalyzer> = None;
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|error| {
            format!(
                "ReplayGain analysis failed creating decoder for {}: {}",
                path.display(),
                error
            )
        })?;

    let mut decoded_any_audio = false;
    loop {
        let packet = match format_reader.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error)) if error.kind() == ErrorKind::UnexpectedEof => {
                break
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder = symphonia::default::get_codecs()
                    .make(&codec_params, &DecoderOptions::default())
                    .map_err(|error| {
                        format!(
                            "ReplayGain analysis failed resetting decoder for {}: {}",
                            path.display(),
                            error
                        )
                    })?;
                continue;
            }
            Err(SymphoniaError::DecodeError(_))
            | Err(SymphoniaError::LimitError(_))
            | Err(SymphoniaError::IoError(_)) => continue,
            Err(other) => {
                return Err(format!(
                    "ReplayGain packet read failed for {}: {}",
                    path.display(),
                    other
                ));
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::LimitError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder = symphonia::default::get_codecs()
                    .make(&codec_params, &DecoderOptions::default())
                    .map_err(|error| {
                        format!(
                            "ReplayGain analysis failed resetting decoder for {}: {}",
                            path.display(),
                            error
                        )
                    })?;
                continue;
            }
            Err(other) => {
                return Err(format!(
                    "ReplayGain decode failed for {}: {}",
                    path.display(),
                    other
                ));
            }
        };

        let spec = decoded.spec();
        let decoded_channel_count = spec.channels.count().max(1);
        if analyzer.is_none() {
            analyzer = Some(
                Rg1PcmAnalyzer::new(spec.rate, decoded_channel_count).map_err(|error| {
                    format!(
                        "ReplayGain analysis initialization failed for {}: {}",
                        path.display(),
                        error
                    )
                })?,
            );
        } else {
            let expected_channels = analyzer
                .as_ref()
                .map(|value| value.channel_count)
                .unwrap_or(0);
            if decoded_channel_count != expected_channels {
                return Err(format!(
                    "ReplayGain analysis channel layout changed while decoding {}",
                    path.display()
                ));
            }
        }

        let duration = decoded.capacity() as u64;
        if duration == 0 {
            continue;
        }

        let mut sample_buffer = SampleBuffer::<f32>::new(duration, *spec);
        sample_buffer.copy_interleaved_ref(decoded);
        if let Some(analyzer) = analyzer.as_mut() {
            analyzer.process_interleaved(sample_buffer.samples());
        }
        decoded_any_audio = true;
    }

    if !decoded_any_audio {
        return Err(format!(
            "ReplayGain analysis decoded no audio frames for {}",
            path.display()
        ));
    }

    let analyzer = analyzer.ok_or_else(|| {
        format!(
            "ReplayGain analysis decoded no supported audio frames for {}",
            path.display()
        )
    })?;
    analyzer.finish().map_err(|error| {
        format!(
            "ReplayGain analysis failed finalizing {}: {}",
            path.display(),
            error
        )
    })
}

pub(crate) fn analyze_album(paths: &[PathBuf]) -> Result<ReplayGainAnalysisValues, String> {
    if paths.is_empty() {
        return Err("Cannot analyze ReplayGain for an empty album selection".to_string());
    }

    let mut analyses = Vec::with_capacity(paths.len());
    for path in paths {
        analyses.push(analyze_track(path)?);
    }
    combine_track_analyses(analyses.iter())
}

#[derive(Debug, Clone, Copy)]
struct R128TrackSummary {
    loudness_lufs: f64,
    peak_linear: f64,
    duration_seconds: f64,
}

fn analyze_track_r128_summary(path: &Path) -> Result<R128TrackSummary, String> {
    let input = File::open(path)
        .map_err(|error| format!("R128 analysis failed opening {}: {}", path.display(), error))?;
    let mss = MediaSourceStream::new(Box::new(input), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }

    let probe = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("R128 analysis failed probing {}: {}", path.display(), error))?;
    let mut format_reader = probe.format;
    let default_track = format_reader
        .default_track()
        .ok_or_else(|| format!("R128 analysis found no audio track in {}", path.display()))?;
    let track_id = default_track.id;
    let codec_params = default_track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|error| {
            format!(
                "R128 analysis failed creating decoder for {}: {}",
                path.display(),
                error
            )
        })?;

    let mut analyzer: Option<EbuR128> = None;
    let mut sample_rate_hz: u32 = 0;
    let mut channel_count: usize = 0;
    let mut processed_frames: u64 = 0;
    let mut decoded_any_audio = false;
    loop {
        let packet = match format_reader.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error)) if error.kind() == ErrorKind::UnexpectedEof => {
                break
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder = symphonia::default::get_codecs()
                    .make(&codec_params, &DecoderOptions::default())
                    .map_err(|error| {
                        format!(
                            "R128 analysis failed resetting decoder for {}: {}",
                            path.display(),
                            error
                        )
                    })?;
                continue;
            }
            Err(SymphoniaError::DecodeError(_))
            | Err(SymphoniaError::LimitError(_))
            | Err(SymphoniaError::IoError(_)) => continue,
            Err(other) => {
                return Err(format!(
                    "R128 packet read failed for {}: {}",
                    path.display(),
                    other
                ));
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::LimitError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder = symphonia::default::get_codecs()
                    .make(&codec_params, &DecoderOptions::default())
                    .map_err(|error| {
                        format!(
                            "R128 analysis failed resetting decoder for {}: {}",
                            path.display(),
                            error
                        )
                    })?;
                continue;
            }
            Err(other) => {
                return Err(format!(
                    "R128 decode failed for {}: {}",
                    path.display(),
                    other
                ));
            }
        };

        let spec = decoded.spec();
        let decoded_channel_count = spec.channels.count().max(1);
        if analyzer.is_none() {
            sample_rate_hz = spec.rate;
            channel_count = decoded_channel_count;
            analyzer = Some(
                EbuR128::new(
                    channel_count as u32,
                    sample_rate_hz,
                    EbuR128Mode::I | EbuR128Mode::SAMPLE_PEAK,
                )
                .map_err(|error| {
                    format!(
                        "R128 analysis initialization failed for {}: {}",
                        path.display(),
                        error
                    )
                })?,
            );
        } else if decoded_channel_count != channel_count {
            return Err(format!(
                "R128 analysis channel layout changed while decoding {}",
                path.display()
            ));
        }

        let duration = decoded.capacity() as u64;
        if duration == 0 {
            continue;
        }

        let mut sample_buffer = SampleBuffer::<f32>::new(duration, *spec);
        sample_buffer.copy_interleaved_ref(decoded);
        if let Some(analyzer) = analyzer.as_mut() {
            analyzer
                .add_frames_f32(sample_buffer.samples())
                .map_err(|error| {
                    format!(
                        "R128 analysis failed processing samples for {}: {}",
                        path.display(),
                        error
                    )
                })?;
        }
        processed_frames = processed_frames.saturating_add(duration);
        decoded_any_audio = true;
    }

    if !decoded_any_audio {
        return Err(format!(
            "R128 analysis decoded no audio frames for {}",
            path.display()
        ));
    }

    let analyzer = analyzer.ok_or_else(|| {
        format!(
            "R128 analysis decoded no supported audio frames for {}",
            path.display()
        )
    })?;
    let loudness_lufs = analyzer.loudness_global().map_err(|error| {
        format!(
            "R128 analysis failed computing loudness for {}: {}",
            path.display(),
            error
        )
    })?;
    if !loudness_lufs.is_finite() {
        return Err(format!(
            "R128 analysis returned non-finite loudness for {}",
            path.display()
        ));
    }

    let mut peak_linear = 0.0f64;
    for channel_index in 0..channel_count {
        let peak = analyzer
            .sample_peak(channel_index as u32)
            .map_err(|error| {
                format!(
                    "R128 analysis failed reading sample peak for {}: {}",
                    path.display(),
                    error
                )
            })?;
        peak_linear = peak_linear.max(peak);
    }
    if !peak_linear.is_finite() || peak_linear <= 0.0 {
        peak_linear = f64::from(f32::EPSILON);
    }

    let duration_seconds = if sample_rate_hz == 0 {
        R128_MIN_DURATION_SECONDS
    } else {
        ((processed_frames as f64) / f64::from(sample_rate_hz)).max(R128_MIN_DURATION_SECONDS)
    };

    Ok(R128TrackSummary {
        loudness_lufs,
        peak_linear,
        duration_seconds,
    })
}

fn values_from_r128_summary(summary: R128TrackSummary) -> Result<ReplayGainAnalysisValues, String> {
    let gain_db = (R128_REFERENCE_LOUDNESS_LUFS - summary.loudness_lufs) as f32;
    if !gain_db.is_finite() {
        return Err("R128 analysis computed a non-finite gain value".to_string());
    }
    let peak = summary.peak_linear as f32;
    if !peak.is_finite() || peak <= 0.0 {
        return Err("R128 analysis computed an invalid peak value".to_string());
    }
    Ok(ReplayGainAnalysisValues { gain_db, peak })
}

fn analyze_track_r128(path: &Path) -> Result<ReplayGainAnalysisValues, String> {
    let summary = analyze_track_r128_summary(path)?;
    values_from_r128_summary(summary)
}

fn analyze_album_r128(paths: &[PathBuf]) -> Result<ReplayGainAnalysisValues, String> {
    if paths.is_empty() {
        return Err("Cannot analyze ReplayGain for an empty album selection".to_string());
    }

    let mut total_weighted_power = 0.0f64;
    let mut total_duration = 0.0f64;
    let mut peak_linear = 0.0f64;

    for path in paths {
        let summary = analyze_track_r128_summary(path)?;
        peak_linear = peak_linear.max(summary.peak_linear);
        let power = 10.0f64.powf(summary.loudness_lufs / 10.0);
        total_weighted_power += power * summary.duration_seconds;
        total_duration += summary.duration_seconds;
    }

    if total_duration <= 0.0 || total_weighted_power <= 0.0 {
        return Err("R128 album analysis produced no valid loudness data".to_string());
    }

    let album_loudness_lufs = (total_weighted_power / total_duration).log10() * 10.0;
    values_from_r128_summary(R128TrackSummary {
        loudness_lufs: album_loudness_lufs,
        peak_linear: peak_linear.max(f64::from(f32::EPSILON)),
        duration_seconds: total_duration,
    })
}

pub(crate) fn analyze_track_values(
    path: &Path,
    loudness_standard: LoudnessStandard,
) -> Result<ReplayGainAnalysisValues, String> {
    match loudness_standard {
        LoudnessStandard::R128 => analyze_track_r128(path),
        LoudnessStandard::ReplayGain1 => analyze_track(path)?.values(),
    }
}

pub(crate) fn analyze_album_values(
    paths: &[PathBuf],
    loudness_standard: LoudnessStandard,
) -> Result<ReplayGainAnalysisValues, String> {
    match loudness_standard {
        LoudnessStandard::R128 => analyze_album_r128(paths),
        LoudnessStandard::ReplayGain1 => analyze_album(paths),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        analyze_album, analyze_track, coefficients_for_sample_rate, combine_track_analyses,
        Rg1PcmAnalyzer,
    };
    use std::f32::consts::PI;
    use std::path::{Path, PathBuf};

    fn generate_sine_interleaved(
        sample_rate_hz: u32,
        channels: usize,
        frequency_hz: f32,
        amplitude: f32,
        duration_seconds: f32,
    ) -> Vec<f32> {
        let total_frames = ((sample_rate_hz as f32) * duration_seconds) as usize;
        let mut samples = Vec::with_capacity(total_frames * channels);
        for frame in 0..total_frames {
            let phase = 2.0 * PI * frequency_hz * (frame as f32) / (sample_rate_hz as f32);
            let value = phase.sin() * amplitude;
            for _ in 0..channels {
                samples.push(value);
            }
        }
        samples
    }

    fn analyze_generated_track(
        sample_rate_hz: u32,
        channels: usize,
        frequency_hz: f32,
        amplitude: f32,
        duration_seconds: f32,
    ) -> super::ReplayGainAnalysisValues {
        let mut analyzer = Rg1PcmAnalyzer::new(sample_rate_hz, channels)
            .expect("failed to create analyzer for generated test signal");
        let samples = generate_sine_interleaved(
            sample_rate_hz,
            channels,
            frequency_hz,
            amplitude,
            duration_seconds,
        );
        analyzer.process_interleaved(&samples);
        analyzer
            .finish()
            .expect("generated signal analysis should finalize")
            .values()
            .expect("generated signal analysis should produce values")
    }

    #[test]
    fn test_rg1_analysis_reports_more_negative_gain_for_louder_signal() {
        let quiet = analyze_generated_track(44_100, 2, 997.0, 0.08, 8.0);
        let loud = analyze_generated_track(44_100, 2, 997.0, 0.40, 8.0);

        assert!(quiet.peak < loud.peak);
        assert!(quiet.gain_db > loud.gain_db);
    }

    #[test]
    fn test_rg1_analysis_handles_short_signals() {
        let values = analyze_generated_track(44_100, 2, 440.0, 0.25, 0.020);
        assert!(values.gain_db.is_finite());
        assert!(values.peak > 0.0);
    }

    #[test]
    fn test_album_values_combine_track_histograms_and_peak() {
        let mut low_analyzer = Rg1PcmAnalyzer::new(48_000, 2).expect("analyzer init failed");
        let low_samples = generate_sine_interleaved(48_000, 2, 330.0, 0.10, 6.0);
        low_analyzer.process_interleaved(&low_samples);
        let low_track = low_analyzer.finish().expect("low track finalize failed");
        let low_values = low_track.values().expect("low track values failed");

        let mut high_analyzer = Rg1PcmAnalyzer::new(48_000, 2).expect("analyzer init failed");
        let high_samples = generate_sine_interleaved(48_000, 2, 330.0, 0.50, 6.0);
        high_analyzer.process_interleaved(&high_samples);
        let high_track = high_analyzer.finish().expect("high track finalize failed");
        let high_values = high_track.values().expect("high track values failed");

        let album_values = combine_track_analyses([&low_track, &high_track])
            .expect("album aggregation should succeed");

        assert!(album_values.peak >= low_values.peak);
        assert!(album_values.peak >= high_values.peak);
        assert!(album_values.gain_db < low_values.gain_db);
    }

    #[test]
    fn test_coefficients_fallback_to_nearest_supported_rate() {
        let coeff = coefficients_for_sample_rate(47_999);
        assert_eq!(coeff.sample_rate_hz, 48_000);
    }

    fn metadata_fixture_dir() -> PathBuf {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir
            .ancestors()
            .map(|ancestor| ancestor.join("tests/fixtures/metadata_preservation"))
            .find(|candidate| candidate.is_dir())
            .unwrap_or_else(|| {
                panic!(
                    "failed to locate metadata fixtures from manifest dir {}",
                    manifest_dir.display()
                )
            })
    }

    #[test]
    fn test_replaygain_analyzer_decodes_metadata_fixtures() {
        let fixtures_dir = metadata_fixture_dir();
        let fixtures: [PathBuf; 9] = [
            fixtures_dir.join("base.aac"),
            fixtures_dir.join("base.flac"),
            fixtures_dir.join("base.m4a"),
            fixtures_dir.join("base.mp3"),
            fixtures_dir.join("base.mp4"),
            fixtures_dir.join("base.ogg"),
            fixtures_dir.join("base.opus"),
            fixtures_dir.join("base.wav"),
            fixtures_dir.join("base.wv"),
        ];

        for fixture in fixtures {
            match analyze_track(&fixture) {
                Ok(track) => {
                    let values = track.values().unwrap_or_else(|error| {
                        panic!(
                            "value extraction failed for {}: {}",
                            fixture.display(),
                            error
                        )
                    });
                    assert!(values.gain_db.is_finite());
                    assert!(values.peak > 0.0);
                    assert!(values.peak <= 2.0);
                }
                Err(error) => {
                    let extension = fixture
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    assert!(
                        matches!(extension.as_str(), "opus" | "wv"),
                        "track analysis failed for {}: {}",
                        fixture.display(),
                        error
                    );
                    assert!(
                        error.contains("unsupported codec") || error.contains("end of stream"),
                        "unexpected decode error for {}: {}",
                        fixture.display(),
                        error
                    );
                }
            }
        }
    }

    #[test]
    fn test_album_analysis_uses_all_paths() {
        let fixtures_dir = metadata_fixture_dir();
        let single = vec![fixtures_dir.join("base.flac")];
        let multiple = vec![
            fixtures_dir.join("base.flac"),
            fixtures_dir.join("base.wav"),
        ];

        let single_values = analyze_album(&single).expect("single-track album should analyze");
        let multiple_values = analyze_album(&multiple).expect("multi-track album should analyze");

        assert!(multiple_values.peak >= single_values.peak);
    }

    #[test]
    fn test_replaygain_baseline_matches_ffmpeg_reference_for_fixture() {
        let fixture = metadata_fixture_dir().join("base.flac");
        let values = analyze_track(&fixture)
            .expect("fixture track analysis should succeed")
            .values()
            .expect("fixture values should be available");

        // FFmpeg `af=replaygain` baseline for this fixture: track_gain=+6.93 dB.
        assert!(
            (values.gain_db - 6.93).abs() <= 0.35,
            "gain was {} dB, expected about +6.93 dB",
            values.gain_db
        );
        assert!(values.peak > 0.0);
    }
}
