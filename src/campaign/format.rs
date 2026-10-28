use framework::Point;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CoordStyle {
    Flat,
    #[default]
    Paren,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParamStyle {
    Flat,
    #[default]
    Paren,
    Labelled,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Format {
    pub coords: CoordStyle,
    pub params: ParamStyle,
}

impl Format {
    pub fn new(coords: CoordStyle, params: ParamStyle) -> Self {
        Format { coords, params }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScenarioBlock {
    pub tau: f64,
    pub k: usize,
    pub antennas: Vec<Point>,
    pub serviced: Vec<u32>,
}

fn num(v: f64) -> String {
    assert!(v.is_finite(), "cannot encode a non-finite coordinate: {v}");
    format!("{v:?}")
}

pub fn encode(blocks: &[ScenarioBlock], format: Format) -> String {
    let mut out = String::new();
    for block in blocks {
        match format.params {
            ParamStyle::Flat => out.push_str(&format!("{},{}\n", num(block.tau), block.k)),
            ParamStyle::Paren => out.push_str(&format!("({},{})\n", num(block.tau), block.k)),
            ParamStyle::Labelled => {
                out.push_str(&format!("tau={},k={}\n", num(block.tau), block.k))
            }
        }
        let coords: Vec<String> = match format.coords {
            CoordStyle::Flat => block
                .antennas
                .iter()
                .flat_map(|p| [num(p.x), num(p.y)])
                .collect(),
            CoordStyle::Paren => block
                .antennas
                .iter()
                .map(|p| format!("({},{})", num(p.x), num(p.y)))
                .collect(),
        };
        out.push_str(&coords.join(","));
        out.push('\n');
        let ids: Vec<String> = block.serviced.iter().map(u32::to_string).collect();
        out.push_str(&ids.join(","));
        out.push('\n');
    }
    out
}

pub fn decode(text: &str) -> Result<Vec<ScenarioBlock>, String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if !lines.len().is_multiple_of(3) {
        return Err(format!("{} lines, not a multiple of 3", lines.len()));
    }

    let mut blocks = Vec::new();
    for (i, chunk) in lines.chunks(3).enumerate() {
        let params = fields(chunk[0]);
        let [tau, k] = params.as_slice() else {
            return Err(format!(
                "block {i}: line 1 has {} fields, expected 2",
                params.len()
            ));
        };
        let tau: f64 = parse(tau, i, "tau")?;
        let k: usize = parse(k, i, "k")?;

        let coords = fields(chunk[1]);
        if coords.len() != 2 * k {
            return Err(format!(
                "block {i}: {} coordinate fields, expected {} for k={k}",
                coords.len(),
                2 * k
            ));
        }
        let mut antennas = Vec::with_capacity(k);
        for pair in coords.chunks(2) {
            antennas.push(Point::new(
                parse(&pair[0], i, "x")?,
                parse(&pair[1], i, "y")?,
            ));
        }

        let mut serviced = Vec::new();
        for field in fields(chunk[2]) {
            serviced.push(parse(&field, i, "building id")?);
        }
        blocks.push(ScenarioBlock {
            tau,
            k,
            antennas,
            serviced,
        });
    }
    Ok(blocks)
}

fn fields(line: &str) -> Vec<String> {
    line.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(|f| {
            let f = f.trim_matches(|c| c == '(' || c == ')');
            f.rsplit('=').next().unwrap_or(f)
        })
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse<T: std::str::FromStr>(field: &str, block: usize, what: &str) -> Result<T, String> {
    field
        .parse()
        .map_err(|_| format!("block {block}: invalid {what}: {field:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> ScenarioBlock {
        ScenarioBlock {
            tau: 0.5,
            k: 2,
            antennas: vec![
                Point::new(0.1 + 0.2, -3.0e7),
                Point::new(f64::MIN_POSITIVE, 1.7976931348623157e308),
            ],
            serviced: vec![7, 11, 4294967295],
        }
    }

    #[test]
    fn round_trips_every_style() {
        for coords in [CoordStyle::Flat, CoordStyle::Paren] {
            for params in [ParamStyle::Flat, ParamStyle::Paren, ParamStyle::Labelled] {
                let text = encode(&[block()], Format::new(coords, params));
                let back = decode(&text).expect("decode failed");
                assert_eq!(back, vec![block()], "{coords:?}/{params:?}");
            }
        }
    }

    #[test]
    fn coordinates_survive_bit_exact() {
        let original = block();
        let text = encode(std::slice::from_ref(&original), Format::default());
        let back = decode(&text).unwrap();
        for (a, b) in original.antennas.iter().zip(&back[0].antennas) {
            assert_eq!(a.x.to_bits(), b.x.to_bits());
            assert_eq!(a.y.to_bits(), b.y.to_bits());
        }
    }

    #[test]
    fn emits_three_lines_per_block() {
        let text = encode(&[block(), block()], Format::default());
        assert_eq!(text.lines().count(), 6);
    }

    #[test]
    fn rejects_truncated_coordinate_list() {
        let text = "0.5,2\n1.0,2.0,3.0\n7\n";
        assert!(decode(text).is_err());
    }

    #[test]
    fn empty_serviced_list_is_valid() {
        let text = encode(
            &[ScenarioBlock {
                tau: 0.25,
                k: 1,
                antennas: vec![Point::new(1.0, 2.0)],
                serviced: vec![],
            }],
            Format::default(),
        );
        assert_eq!(decode(&text).unwrap()[0].serviced, Vec::<u32>::new());
    }
}
