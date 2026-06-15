/// Haversine distance between two WGS84 points, meters.
pub fn haversine_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let lat1 = lat1.to_radians();
    let lat2 = lat2.to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
}

/// Initial bearing (lon1,lat1)→(lon2,lat2), degrees clockwise from north, [0, 360).
pub fn bearing_deg(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let lat1 = lat1.to_radians();
    let lat2 = lat2.to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

/// Result of projecting a query point onto a polyline.
#[derive(Debug, Clone)]
pub struct Projection {
    /// Arc-length from polyline start to the projected point, meters.
    pub arc_offset_m: f64,
    /// Projected point (lon, lat).
    pub point: (f64, f64),
    /// Distance from the query point to the projected point, meters.
    pub distance_m: f64,
}

/// Project `(lon, lat)` onto the nearest point of `vertices`.
/// Returns `None` if `vertices` has fewer than 2 points.
pub fn project_onto_polyline(lon: f64, lat: f64, vertices: &[(f64, f64)]) -> Option<Projection> {
    if vertices.len() < 2 {
        return None;
    }
    let mut best: Option<Projection> = None;
    let mut cumulative_m = 0.0_f64;

    for w in vertices.windows(2) {
        let (ax, ay) = w[0];
        let (bx, by) = w[1];
        let seg_len = haversine_m(ax, ay, bx, by);

        // Approximate planar projection (adequate for short segments ≤ a few km).
        let cos_lat = ((ay + by) / 2.0).to_radians().cos();
        let ddx = (bx - ax) * cos_lat;
        let ddy = by - ay;
        let seg_sq = ddx * ddx + ddy * ddy;

        let (px, py, t) = if seg_sq < 1e-20 {
            (ax, ay, 0.0_f64)
        } else {
            let qx = (lon - ax) * cos_lat;
            let qy = lat - ay;
            let t = ((qx * ddx + qy * ddy) / seg_sq).clamp(0.0, 1.0);
            (ax + t * (bx - ax), ay + t * (by - ay), t)
        };

        let dist = haversine_m(lon, lat, px, py);
        let arc = cumulative_m + t * seg_len;

        if best.as_ref().map_or(true, |b| dist < b.distance_m) {
            best = Some(Projection { arc_offset_m: arc, point: (px, py), distance_m: dist });
        }
        cumulative_m += seg_len;
    }
    best
}

/// Total arc length of a polyline, meters.
pub fn polyline_length_m(vertices: &[(f64, f64)]) -> f64 {
    vertices
        .windows(2)
        .map(|w| haversine_m(w[0].0, w[0].1, w[1].0, w[1].1))
        .sum()
}

/// Bearing at `arc_offset_m` along `vertices`, measured over a 20 m window.
/// `forward = true` → use the window ahead of the offset (start LRPs).
/// `forward = false` → use the window behind the offset (last LRP, per spec §8).
pub fn bearing_at_offset(vertices: &[(f64, f64)], arc_offset_m: f64, forward: bool) -> f64 {
    const WINDOW_M: f64 = 20.0;
    if vertices.len() < 2 {
        return 0.0;
    }
    let total = polyline_length_m(vertices);
    let (ws, we) = if forward {
        let s = arc_offset_m.clamp(0.0, total);
        let e = (arc_offset_m + WINDOW_M).clamp(0.0, total);
        (s, e)
    } else {
        let e = arc_offset_m.clamp(0.0, total);
        let s = (arc_offset_m - WINDOW_M).clamp(0.0, total);
        (s, e)
    };

    let p_start = interpolate_at(vertices, ws);
    let p_end = interpolate_at(vertices, we);

    // Degenerate: window collapsed to a point — fall back to overall direction.
    if (p_start.0 - p_end.0).hypot(p_start.1 - p_end.1) < 1e-12 {
        let first = vertices[0];
        let last = *vertices.last().unwrap();
        return bearing_deg(first.0, first.1, last.0, last.1);
    }

    if forward {
        bearing_deg(p_start.0, p_start.1, p_end.0, p_end.1)
    } else {
        bearing_deg(p_end.0, p_end.1, p_start.0, p_start.1)
    }
}

/// Interpolate a point at arc-length `offset_m` along `vertices`.
pub fn interpolate_at(vertices: &[(f64, f64)], offset_m: f64) -> (f64, f64) {
    let mut rem = offset_m;
    for w in vertices.windows(2) {
        let (ax, ay) = w[0];
        let (bx, by) = w[1];
        let len = haversine_m(ax, ay, bx, by);
        if rem <= len || len < 1e-12 {
            let t = if len > 0.0 { (rem / len).clamp(0.0, 1.0) } else { 0.0 };
            return (ax + t * (bx - ax), ay + t * (by - ay));
        }
        rem -= len;
    }
    *vertices.last().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_equator() {
        // ~111 km per degree of longitude at equator
        let d = haversine_m(0.0, 0.0, 1.0, 0.0);
        assert!((d - 111_195.0).abs() < 200.0, "d={d}");
    }

    #[test]
    fn bearing_north() {
        assert!((bearing_deg(0.0, 0.0, 0.0, 1.0) - 0.0).abs() < 0.01);
    }

    #[test]
    fn bearing_east() {
        let b = bearing_deg(0.0, 0.0, 1.0, 0.0);
        assert!((b - 90.0).abs() < 0.1, "b={b}");
    }

    #[test]
    fn project_midpoint() {
        let verts = vec![(0.0_f64, 0.0_f64), (0.0, 1.0)];
        let proj = project_onto_polyline(0.001, 0.5, &verts).unwrap();
        assert!((proj.point.1 - 0.5).abs() < 0.001);
        assert!(proj.distance_m < 200.0);
    }

    #[test]
    fn project_clamps_to_endpoint() {
        let verts = vec![(0.0_f64, 0.0_f64), (0.0, 1.0)];
        let proj = project_onto_polyline(0.001, 2.0, &verts).unwrap();
        // Should clamp to (0,1)
        assert!((proj.point.1 - 1.0).abs() < 0.001);
    }
}
