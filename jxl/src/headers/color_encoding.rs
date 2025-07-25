// Copyright (c) the JPEG XL Project Authors. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use crate::{bit_reader::BitReader, error::Error, headers::encodings::*};
use jxl_macros::UnconditionalCoder;
use num_derive::FromPrimitive;
use std::fmt;

use md5::Context;

// Define type aliases for clarity
pub type Matrix3x3<T> = [[T; 3]; 3];
pub type Vector3<T> = [T; 3];

// Bradford matrices for chromatic adaptation
const K_BRADFORD: Matrix3x3<f64> = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

const K_BRADFORD_INV: Matrix3x3<f64> = [
    [0.9869929, -0.1470543, 0.1599627],
    [0.4323053, 0.5183603, 0.0492912],
    [-0.0085287, 0.0400428, 0.9684867],
];

fn mul_3x3_vector(matrix: &Matrix3x3<f64>, vector: &Vector3<f64>) -> Vector3<f64> {
    std::array::from_fn(|i| {
        matrix[i]
            .iter()
            .zip(vector.iter())
            .map(|(&matrix_element, &vector_element)| matrix_element * vector_element)
            .sum()
    })
}

fn mul_3x3_matrix(mat1: &Matrix3x3<f64>, mat2: &Matrix3x3<f64>) -> Matrix3x3<f64> {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| mat1[i][k] * mat2[k][j]).sum()))
}

fn det2x2(a: f64, b: f64, c: f64, d: f64) -> f64 {
    a * d - b * c
}

fn calculate_cofactor(m: &Matrix3x3<f64>, r: usize, c: usize) -> f64 {
    // Determine the actual row and column indices for the 2x2 submatrix
    // by excluding the current row 'r' and column 'c'.
    // Ensure they are taken in ascending order to form the submatrix consistently.
    let mut sub_rows = [0; 2];
    let mut sub_cols = [0; 2];

    let mut current_idx = 0;
    for i in 0..3 {
        if i != r {
            sub_rows[current_idx] = i;
            current_idx += 1;
        }
    }

    current_idx = 0;
    for i in 0..3 {
        if i != c {
            sub_cols[current_idx] = i;
            current_idx += 1;
        }
    }

    let minor_val = det2x2(
        m[sub_rows[0]][sub_cols[0]],
        m[sub_rows[0]][sub_cols[1]],
        m[sub_rows[1]][sub_cols[0]],
        m[sub_rows[1]][sub_cols[1]],
    );

    // Apply the checkerboard pattern sign for the cofactor
    if (r + c) % 2 == 0 {
        minor_val
    } else {
        -minor_val
    }
}

/// Calculates the inverse of a 3x3 matrix.
fn inv_3x3_matrix(m: &Matrix3x3<f64>) -> Result<Matrix3x3<f64>, Error> {
    let cofactor_matrix: [[f64; 3]; 3] = std::array::from_fn(|r_idx| {
        std::array::from_fn(|c_idx| calculate_cofactor(m, r_idx, c_idx))
    });

    let det = m[0]
        .iter()
        .zip(cofactor_matrix[0].iter())
        .map(|(&m_element, &cof_element)| m_element * cof_element)
        .sum::<f64>();

    // Check for numerical singularity.
    const EPSILON: f64 = 1e-12;
    if det.abs() < EPSILON {
        return Err(Error::MatrixInversionFailed(det.abs()));
    }

    let inv_det = 1.0 / det;

    let adjugate_matrix: [[f64; 3]; 3] =
        std::array::from_fn(|r_idx| std::array::from_fn(|c_idx| cofactor_matrix[c_idx][r_idx]));

    // Inverse matrix = (1/det) * Adjugate matrix.
    Ok(std::array::from_fn(|r_idx| {
        std::array::from_fn(|c_idx| adjugate_matrix[r_idx][c_idx] * inv_det)
    }))
}

#[allow(clippy::too_many_arguments)]
pub fn primaries_to_xyz(
    rx: f32,
    ry: f32,
    gx: f32,
    gy: f32,
    bx: f32,
    by: f32,
    wx: f32,
    wy: f32,
) -> Result<Matrix3x3<f64>, Error> {
    // Validate white point coordinates
    if !((0.0..=1.0).contains(&wx) && (wy > 0.0 && wy <= 1.0)) {
        return Err(Error::IccInvalidWhitePoint(
            wx,
            wy,
            "White point coordinates out of range ([0,1] for x, (0,1] for y)".to_string(),
        ));
    }
    // Comment from libjxl:
    // TODO(lode): also require rx, ry, gx, gy, bx, to be in range 0-1? ICC
    // profiles in theory forbid negative XYZ values, but in practice the ACES P0
    // color space uses a negative y for the blue primary.

    // Construct the primaries matrix P. Its columns are the XYZ coordinates
    // of the R, G, B primaries (derived from their chromaticities x, y, z=1-x-y).
    // P = [[xr, xg, xb],
    //      [yr, yg, yb],
    //      [zr, zg, zb]]
    let rz = 1.0 - rx as f64 - ry as f64;
    let gz = 1.0 - gx as f64 - gy as f64;
    let bz = 1.0 - bx as f64 - by as f64;
    let p_matrix = [
        [rx as f64, gx as f64, bx as f64],
        [ry as f64, gy as f64, by as f64],
        [rz, gz, bz],
    ];

    let p_inv_matrix = inv_3x3_matrix(&p_matrix)?;

    // Convert reference white point (wx, wy) to XYZ form with Y=1
    // This is WhitePoint_XYZ_wp = [wx/wy, 1, (1-wx-wy)/wy]
    let x_over_y_wp = wx as f64 / wy as f64;
    let z_over_y_wp = (1.0 - wx as f64 - wy as f64) / wy as f64;

    if !x_over_y_wp.is_finite() || !z_over_y_wp.is_finite() {
        return Err(Error::IccInvalidWhitePoint(
            wx,
            wy,
            "Calculated X/Y or Z/Y for white point is not finite.".to_string(),
        ));
    }
    let white_point_xyz_vec: Vector3<f64> = [x_over_y_wp, 1.0, z_over_y_wp];

    // Calculate scaling factors S = [Sr, Sg, Sb] such that P * S = WhitePoint_XYZ_wp
    // So, S = P_inv * WhitePoint_XYZ_wp
    let s_vec = mul_3x3_vector(&p_inv_matrix, &white_point_xyz_vec);

    // Construct diagonal matrix S_diag from s_vec
    let s_diag_matrix = [
        [s_vec[0], 0.0, 0.0],
        [0.0, s_vec[1], 0.0],
        [0.0, 0.0, s_vec[2]],
    ];
    // The final RGB-to-XYZ matrix is P * S_diag
    let result_matrix = mul_3x3_matrix(&p_matrix, &s_diag_matrix);

    Ok(result_matrix)
}

fn adapt_to_xyz_d50(wx: f32, wy: f32) -> Result<Matrix3x3<f64>, Error> {
    if !((0.0..=1.0).contains(&wx) && (wy > 0.0 && wy <= 1.0)) {
        return Err(Error::IccInvalidWhitePoint(
            wx,
            wy,
            "White point coordinates out of range ([0,1] for x, (0,1] for y)".to_string(),
        ));
    }

    // Convert white point (wx, wy) to XYZ with Y=1
    let x_over_y = wx as f64 / wy as f64;
    let z_over_y = (1.0 - wx as f64 - wy as f64) / wy as f64;

    // Check for finiteness, as 1.0 / tiny float can overflow.
    if !x_over_y.is_finite() || !z_over_y.is_finite() {
        return Err(Error::IccInvalidWhitePoint(
            wx,
            wy,
            "Calculated X/Y or Z/Y for white point is not finite.".to_string(),
        ));
    }
    let w: Vector3<f64> = [x_over_y, 1.0, z_over_y];

    // D50 white point in XYZ (Y=1 form)
    // These are X_D50/Y_D50, 1.0, Z_D50/Y_D50
    let w50: Vector3<f64> = [0.96422, 1.0, 0.82521];

    // Transform to LMS color space
    let lms_source = mul_3x3_vector(&K_BRADFORD, &w);
    let lms_d50 = mul_3x3_vector(&K_BRADFORD, &w50);

    // Check for invalid LMS values which would lead to division by zero
    if lms_source.contains(&0.0) {
        return Err(Error::IccInvalidWhitePoint(
            wx,
            wy,
            "LMS components for source white point are zero, leading to division by zero."
                .to_string(),
        ));
    }

    // Create diagonal scaling matrix in LMS space
    let mut a_diag_matrix: Matrix3x3<f64> = [[0.0; 3]; 3];
    for i in 0..3 {
        a_diag_matrix[i][i] = lms_d50[i] / lms_source[i];
        if !a_diag_matrix[i][i].is_finite() {
            return Err(Error::IccInvalidWhitePoint(
                wx,
                wy,
                format!("Diagonal adaptation matrix component {i} is not finite."),
            ));
        }
    }

    // Combine transformations
    let b_matrix = mul_3x3_matrix(&a_diag_matrix, &K_BRADFORD);
    let final_adaptation_matrix = mul_3x3_matrix(&K_BRADFORD_INV, &b_matrix);

    Ok(final_adaptation_matrix)
}

#[allow(clippy::too_many_arguments)]
pub fn primaries_to_xyz_d50(
    rx: f32,
    ry: f32,
    gx: f32,
    gy: f32,
    bx: f32,
    by: f32,
    wx: f32,
    wy: f32,
) -> Result<Matrix3x3<f64>, Error> {
    // Get the matrix to convert RGB to XYZ, adapted to its native white point (wx, wy).
    let rgb_to_xyz_native_wp_matrix = primaries_to_xyz(rx, ry, gx, gy, bx, by, wx, wy)?;

    // Get the chromatic adaptation matrix from the native white point (wx, wy) to D50.
    let adaptation_to_d50_matrix = adapt_to_xyz_d50(wx, wy)?;
    // This matrix converts XYZ values relative to white point (wx, wy)
    // to XYZ values relative to D50.

    // Combine the matrices: M_RGBtoD50XYZ = M_AdaptToD50 * M_RGBtoNativeXYZ
    // Applying M_RGBtoNativeXYZ first gives XYZ relative to native white point.
    // Then applying M_AdaptToD50 converts these XYZ values to be relative to D50.
    let result_matrix = mul_3x3_matrix(&adaptation_to_d50_matrix, &rgb_to_xyz_native_wp_matrix);

    Ok(result_matrix)
}

#[allow(clippy::too_many_arguments)]
pub fn create_icc_rgb_matrix(
    rx: f32,
    ry: f32,
    gx: f32,
    gy: f32,
    bx: f32,
    by: f32,
    wx: f32,
    wy: f32,
) -> Result<Matrix3x3<f32>, Error> {
    // TODO: think about if we need/want to change precision to f64 for some calculations here
    let result_f64 = primaries_to_xyz_d50(rx, ry, gx, gy, bx, by, wx, wy)?;
    Ok(std::array::from_fn(|r_idx| {
        std::array::from_fn(|c_idx| result_f64[r_idx][c_idx] as f32)
    }))
}

#[allow(clippy::upper_case_acronyms)]
#[derive(UnconditionalCoder, Copy, Clone, PartialEq, Debug, FromPrimitive)]
pub enum ColorSpace {
    RGB,
    Gray,
    XYB,
    Unknown,
}

impl fmt::Display for ColorSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ColorSpace::RGB => "RGB",
                ColorSpace::Gray => "Gra",
                ColorSpace::XYB => "XYB",
                ColorSpace::Unknown => "CS?",
            }
        )
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(UnconditionalCoder, Copy, Clone, PartialEq, Debug, FromPrimitive)]
pub enum WhitePoint {
    D65 = 1,
    Custom = 2,
    E = 10,
    DCI = 11,
}

impl fmt::Display for WhitePoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                WhitePoint::D65 => "D65",
                WhitePoint::Custom => "Cst",
                WhitePoint::E => "EER",
                WhitePoint::DCI => "DCI",
            }
        )
    }
}

impl WhitePoint {
    pub fn to_xy_coords(&self, custom_xy_data: Option<&CustomXY>) -> Result<(f32, f32), Error> {
        match self {
            WhitePoint::Custom => custom_xy_data
                .map(|data| data.as_f32_coords())
                .ok_or(Error::MissingCustomWhitePointData),
            WhitePoint::D65 => Ok((0.3127, 0.3290)),
            WhitePoint::DCI => Ok((0.314, 0.351)),
            WhitePoint::E => Ok((1.0 / 3.0, 1.0 / 3.0)),
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(UnconditionalCoder, Copy, Clone, PartialEq, Debug, FromPrimitive)]
pub enum Primaries {
    SRGB = 1,
    Custom = 2,
    BT2100 = 9,
    P3 = 11,
}

impl fmt::Display for Primaries {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Primaries::SRGB => "SRG",
                Primaries::Custom => "Cst", // Base string for Custom
                Primaries::BT2100 => "202",
                Primaries::P3 => "DCI",
            }
        )
    }
}

impl Primaries {
    pub fn to_xy_coords(
        &self,
        custom_xy_data: Option<&[CustomXY; 3]>,
    ) -> Result<[(f32, f32); 3], Error> {
        match self {
            Primaries::Custom => {
                let data = custom_xy_data.ok_or(Error::MissingCustomPrimariesData)?;
                Ok([
                    data[0].as_f32_coords(),
                    data[1].as_f32_coords(),
                    data[2].as_f32_coords(),
                ])
            }
            Primaries::SRGB => Ok([
                // libjxl has these weird numbers for some reason.
                (0.639_998_7, 0.330_010_15),
                //(0.640, 0.330), // R
                (0.300_003_8, 0.600_003_36),
                //(0.300, 0.600), // G
                (0.150_002_05, 0.059_997_204),
                //(0.150, 0.060), // B
            ]),
            Primaries::BT2100 => Ok([
                (0.708, 0.292), // R
                (0.170, 0.797), // G
                (0.131, 0.046), // B
            ]),
            Primaries::P3 => Ok([
                (0.680, 0.320), // R
                (0.265, 0.690), // G
                (0.150, 0.060), // B
            ]),
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(UnconditionalCoder, Copy, Clone, PartialEq, Debug, FromPrimitive)]
pub enum TransferFunction {
    BT709 = 1,
    Unknown = 2,
    Linear = 8,
    SRGB = 13,
    PQ = 16,
    DCI = 17,
    HLG = 18,
}

impl fmt::Display for TransferFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                TransferFunction::BT709 => "709",
                TransferFunction::Unknown => "TF?",
                TransferFunction::Linear => "Lin",
                TransferFunction::SRGB => "SRG",
                TransferFunction::PQ => "PeQ",
                TransferFunction::DCI => "DCI",
                TransferFunction::HLG => "HLG",
            }
        )
    }
}

#[derive(UnconditionalCoder, Copy, Clone, PartialEq, Debug, FromPrimitive)]
pub enum RenderingIntent {
    Perceptual = 0,
    Relative,
    Saturation,
    Absolute,
}

impl fmt::Display for RenderingIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                RenderingIntent::Perceptual => "Per",
                RenderingIntent::Relative => "Rel",
                RenderingIntent::Saturation => "Sat",
                RenderingIntent::Absolute => "Abs",
            }
        )
    }
}

#[derive(UnconditionalCoder, Debug, Clone)]
pub struct CustomXY {
    #[default(0)]
    #[coder(u2S(Bits(19), Bits(19) + 524288, Bits(20) + 1048576, Bits(21) + 2097152))]
    pub x: i32,
    #[default(0)]
    #[coder(u2S(Bits(19), Bits(19) + 524288, Bits(20) + 1048576, Bits(21) + 2097152))]
    pub y: i32,
}

impl CustomXY {
    /// Converts the stored scaled integer coordinates to f32 (x, y) values.
    pub fn as_f32_coords(&self) -> (f32, f32) {
        (self.x as f32 / 1_000_000.0, self.y as f32 / 1_000_000.0)
    }

    pub fn from_f32_coords(x: f32, y: f32) -> Self {
        Self {
            x: (x * 1_000_000.0).round() as i32,
            y: (y * 1_000_000.0).round() as i32,
        }
    }
}

pub struct CustomTransferFunctionNonserialized {
    color_space: ColorSpace,
}

#[derive(UnconditionalCoder, Debug, Clone)]
#[nonserialized(CustomTransferFunctionNonserialized)]
#[validate]
pub struct CustomTransferFunction {
    #[condition(nonserialized.color_space != ColorSpace::XYB)]
    #[default(false)]
    pub have_gamma: bool,
    #[condition(have_gamma)]
    #[default(3333333)] // XYB gamma
    #[coder(Bits(24))]
    pub gamma: u32,
    #[condition(!have_gamma && nonserialized.color_space != ColorSpace::XYB)]
    #[default(TransferFunction::SRGB)]
    pub transfer_function: TransferFunction,
}

impl CustomTransferFunction {
    #[cfg(test)]
    pub fn empty() -> CustomTransferFunction {
        CustomTransferFunction {
            have_gamma: false,
            gamma: 0,
            transfer_function: TransferFunction::Unknown,
        }
    }
    pub fn gamma(&self) -> f32 {
        assert!(self.have_gamma);
        self.gamma as f32 * 0.0000001
    }

    pub fn check(&self, _: &CustomTransferFunctionNonserialized) -> Result<(), Error> {
        if self.have_gamma {
            let gamma = self.gamma();
            if gamma > 1.0 || gamma * 8192.0 < 1.0 {
                Err(Error::InvalidGamma(gamma))
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }
}

/// Writes a u32 value in big-endian format to the slice at the given position.
pub fn write_u32_be(slice: &mut [u8], pos: usize, value: u32) -> Result<(), Error> {
    if pos.checked_add(4).is_none_or(|end| end > slice.len()) {
        return Err(Error::IccWriteOutOfBounds);
    }
    slice[pos..pos + 4].copy_from_slice(&value.to_be_bytes());
    Ok(())
}

/// Writes a u16 value in big-endian format to the slice at the given position.
pub fn write_u16_be(slice: &mut [u8], pos: usize, value: u16) -> Result<(), Error> {
    if pos.checked_add(2).is_none_or(|end| end > slice.len()) {
        return Err(Error::IccWriteOutOfBounds);
    }
    slice[pos..pos + 2].copy_from_slice(&value.to_be_bytes());
    Ok(())
}

/// Writes a u8 value to the slice at the given position.
pub fn write_u8(slice: &mut [u8], pos: usize, value: u8) -> Result<(), Error> {
    if pos.checked_add(1).is_none_or(|end| end > slice.len()) {
        return Err(Error::IccWriteOutOfBounds);
    }
    slice[pos] = value;
    Ok(())
}

/// Writes a 4-character ASCII tag string to the slice at the given position.
pub fn write_icc_tag(slice: &mut [u8], pos: usize, tag_str: &str) -> Result<(), Error> {
    if tag_str.len() != 4 || !tag_str.is_ascii() {
        return Err(Error::IccInvalidTagString(tag_str.to_string()));
    }
    if pos.checked_add(4).is_none_or(|end| end > slice.len()) {
        return Err(Error::IccWriteOutOfBounds);
    }
    slice[pos..pos + 4].copy_from_slice(tag_str.as_bytes());
    Ok(())
}

/// Creates an ICC 'mluc' tag with a single "enUS" record.
///
/// The input `text` must be ASCII, as it will be encoded as UTF-16BE by prepending
/// a null byte to each ASCII character.
pub fn create_icc_mluc_tag(tags: &mut Vec<u8>, text: &str) -> Result<(), Error> {
    // libjxl comments that "The input text must be ASCII".
    // We enforce this.
    if !text.is_ascii() {
        return Err(Error::IccMlucTextNotAscii(text.to_string()));
    }
    // Tag signature 'mluc' (4 bytes)
    tags.extend_from_slice(b"mluc");
    // Reserved, must be 0 (4 bytes)
    tags.extend_from_slice(&0u32.to_be_bytes());
    // Number of records (u32, 4 bytes) - Hardcoded to 1.
    tags.extend_from_slice(&1u32.to_be_bytes());
    // Record size (u32, 4 bytes) - Each record descriptor is 12 bytes.
    // (Language Code [2] + Country Code [2] + String Length [4] + String Offset [4])
    tags.extend_from_slice(&12u32.to_be_bytes());
    // Language Code (2 bytes) - "en" for English
    tags.extend_from_slice(b"en");
    // Country Code (2 bytes) - "US" for United States
    tags.extend_from_slice(b"US");
    // Length of the string (u32, 4 bytes)
    // For ASCII text encoded as UTF-16BE, each char becomes 2 bytes.
    let string_actual_byte_length = text.len() * 2;
    tags.extend_from_slice(&(string_actual_byte_length as u32).to_be_bytes());
    // Offset of the string (u32, 4 bytes)
    // The string data for this record starts at offset 28.
    tags.extend_from_slice(&28u32.to_be_bytes());
    // The actual string data, encoded as UTF-16BE.
    // For ASCII char 'X', UTF-16BE is 0x00 0x58.
    for ascii_char_code in text.as_bytes() {
        tags.push(0u8);
        tags.push(*ascii_char_code);
    }

    Ok(())
}

pub struct TagInfo {
    signature: [u8; 4],
    // Offset of this tag's data relative to the START of the `tags_data` block
    offset_in_tags_blob: u32,
    // Unpadded size of this tag's actual data content.
    size_unpadded: u32,
}

fn pad_to_4_byte_boundary(data: &mut Vec<u8>) {
    data.resize(data.len().next_multiple_of(4), 0u8);
}

/// Converts an f32 to s15Fixed16 format and appends it as big-endian bytes.
/// s15Fixed16 is a signed 32-bit number with 1 sign bit, 15 integer bits,
/// and 16 fractional bits.
fn append_s15_fixed_16(tags_data: &mut Vec<u8>, value: f32) -> Result<(), Error> {
    // In libjxl, the following specific range check is used: (-32767.995f <= value) && (value <= 32767.995f)
    // This is slightly tighter than the theoretical max positive s15.16 value.
    // We replicate this for consistency.
    if !(value.is_finite() && (-32767.995..=32767.995).contains(&value)) {
        return Err(Error::IccValueOutOfRangeS15Fixed16(value));
    }

    // Multiply by 2^16 and round to nearest integer
    let scaled_value = (value * 65536.0).round();
    // Cast to i32 for correct two's complement representation
    let int_value = scaled_value as i32;
    tags_data.extend_from_slice(&int_value.to_be_bytes());
    Ok(())
}

/// Creates the data for an ICC 'XYZ ' tag and appends it to `tags_data`.
/// The 'XYZ ' tag contains three s15Fixed16Number values.
fn create_icc_xyz_tag(tags_data: &mut Vec<u8>, xyz_color: &[f32; 3]) -> Result<TagInfo, Error> {
    // Tag signature 'XYZ ' (4 bytes, note the trailing space)
    let start_offset = tags_data.len() as u32;
    let signature = b"XYZ ";
    tags_data.extend_from_slice(signature);

    // Reserved, must be 0 (4 bytes)
    tags_data.extend_from_slice(&0u32.to_be_bytes());

    // XYZ data (3 * s15Fixed16Number = 3 * 4 bytes)
    for &val in xyz_color {
        append_s15_fixed_16(tags_data, val)?;
    }

    Ok(TagInfo {
        signature: *b"wtpt",
        offset_in_tags_blob: start_offset,
        size_unpadded: (tags_data.len() as u32) - start_offset,
    })
}

pub fn create_icc_chad_tag(
    tags_data: &mut Vec<u8>,
    chad_matrix: &Matrix3x3<f32>,
) -> Result<TagInfo, Error> {
    // The tag type signature "sf32" (4 bytes).
    let signature = b"sf32";
    let start_offset = tags_data.len() as u32;
    tags_data.extend_from_slice(signature);

    // A reserved field (4 bytes), which must be set to 0.
    tags_data.extend_from_slice(&0u32.to_be_bytes());

    // The 9 matrix elements as s15Fixed16Number values.
    // m[0][0], m[0][1], m[0][2], m[1][0], ..., m[2][2]
    for row_array in chad_matrix.iter() {
        for &value in row_array.iter() {
            append_s15_fixed_16(tags_data, value)?;
        }
    }
    Ok(TagInfo {
        signature: *b"chad",
        offset_in_tags_blob: start_offset,
        size_unpadded: (tags_data.len() as u32) - start_offset,
    })
}

/// Converts CIE xy white point coordinates to CIE XYZ values (Y is normalized to 1.0).
fn cie_xyz_from_white_cie_xy(wx: f32, wy: f32) -> Result<[f32; 3], Error> {
    // Check for wy being too close to zero to prevent division by zero or extreme values.
    if wy.abs() < 1e-12 {
        return Err(Error::IccInvalidWhitePointY(wy));
    }
    let factor = 1.0 / wy;
    let x_val = wx * factor;
    let y_val = 1.0f32;
    let z_val = (1.0 - wx - wy) * factor;
    Ok([x_val, y_val, z_val])
}

/// Creates the data for an ICC `para` (parametricCurveType) tag.
/// It writes `12 + 4 * params.len()` bytes.
fn create_icc_curv_para_tag(
    tags_data: &mut Vec<u8>,
    params: &[f32],
    curve_type: u16,
) -> Result<u32, Error> {
    let start_offset = tags_data.len();
    // Tag type 'para' (4 bytes)
    tags_data.extend_from_slice(b"para");
    // Reserved, must be 0 (4 bytes)
    tags_data.extend_from_slice(&0u32.to_be_bytes());
    // Function type (u16, 2 bytes)
    tags_data.extend_from_slice(&curve_type.to_be_bytes());
    // Reserved, must be 0 (u16, 2 bytes)
    tags_data.extend_from_slice(&0u16.to_be_bytes());
    // Parameters (s15Fixed16Number each)
    for &param in params {
        append_s15_fixed_16(tags_data, param)?;
    }
    Ok((tags_data.len() - start_offset) as u32)
}

pub fn display_from_encoded_pq(display_intensity_target: f32, mut e: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = (2523.0 / 4096.0) * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = (2413.0 / 4096.0) * 32.0;
    const C3: f64 = (2392.0 / 4096.0) * 32.0;
    // Handle the zero case directly.
    if e == 0.0 {
        return 0.0;
    }

    // Handle negative inputs by using their absolute
    // value for the calculation and reapplying the sign at the end.
    let original_sign = e.signum();
    e = e.abs();

    // Core PQ EOTF formula from ST 2084.
    let xp = e.powf(1.0 / M2);
    let num = (xp - C1).max(0.0);
    let den = C2 - C3 * xp;

    // In release builds, a zero denominator would lead to `inf` or `NaN`,
    // which is handled by the assertion below. For valid inputs (e in [0,1]),
    // the denominator is always positive.
    debug_assert!(den != 0.0, "PQ transfer function denominator is zero.");

    let d = (num / den).powf(1.0 / M1);

    // The result `d` should always be non-negative for non-negative inputs.
    debug_assert!(
        d >= 0.0,
        "PQ intermediate value `d` should not be negative."
    );

    // The libjxl implementation includes a scaling factor. Note that `d` represents
    // a value normalized to a 10,000 nit peak.
    let scaled_d = d * (10000.0 / display_intensity_target as f64);

    // Re-apply the original sign.
    scaled_d.copysign(original_sign)
}

/// TF_HLG_Base class for BT.2100 HLG.
///
/// This struct provides methods to convert between non-linear encoded HLG signals
/// and linear display-referred light, following the definitions in BT.2100-2.
///
/// - **"display"**: linear light, normalized to [0, 1].
/// - **"encoded"**: a non-linear HLG signal, nominally in [0, 1].
/// - **"scene"**: scene-referred linear light, normalized to [0, 1].
///
/// The functions are designed to be unbounded to handle inputs outside the
/// nominal [0, 1] range, which can occur during color space conversions. Negative
/// inputs are handled by mirroring the function (`f(-x) = -f(x)`).
#[allow(non_camel_case_types)]
pub struct TF_HLG;

impl TF_HLG {
    // Constants for the HLG formula, as defined in BT.2100.
    const A: f64 = 0.17883277;
    const RA: f64 = 1.0 / Self::A;
    const B: f64 = 1.0 - 4.0 * Self::A;
    const C: f64 = 0.5599107295;
    const INV_12: f64 = 1.0 / 12.0;

    /// Converts a non-linear encoded signal to a linear display value (EOTF).
    ///
    /// This corresponds to `DisplayFromEncoded(e) = OOTF(InvOETF(e))`.
    /// Since the OOTF is simplified to an identity function, this is equivalent
    /// to calling `inv_oetf(e)`.
    #[inline]
    pub fn display_from_encoded(e: f64) -> f64 {
        Self::inv_oetf(e)
    }

    /// Converts a linear display value to a non-linear encoded signal (inverse EOTF).
    ///
    /// This corresponds to `EncodedFromDisplay(d) = OETF(InvOOTF(d))`.
    /// Since the InvOOTF is an identity function, this is equivalent to `oetf(d)`.
    #[inline]
    pub fn encoded_from_display(d: f64) -> f64 {
        Self::oetf(d)
    }

    /// The private HLG OETF, converting scene-referred light to a non-linear signal.
    fn oetf(mut s: f64) -> f64 {
        if s == 0.0 {
            return 0.0;
        }
        let original_sign = s.signum();
        s = s.abs();

        let e = if s <= Self::INV_12 {
            (3.0 * s).sqrt()
        } else {
            Self::A * (12.0 * s - Self::B).ln() + Self::C
        };

        // The result should be positive for positive inputs.
        debug_assert!(e > 0.0);

        e.copysign(original_sign)
    }

    /// The private HLG inverse OETF, converting a non-linear signal back to scene-referred light.
    fn inv_oetf(mut e: f64) -> f64 {
        if e == 0.0 {
            return 0.0;
        }
        let original_sign = e.signum();
        e = e.abs();

        let s = if e <= 0.5 {
            // The `* (1.0 / 3.0)` is slightly more efficient than `/ 3.0`.
            e * e * (1.0 / 3.0)
        } else {
            (((e - Self::C) * Self::RA).exp() + Self::B) * Self::INV_12
        };

        // The result should be non-negative for non-negative inputs.
        debug_assert!(s >= 0.0);

        s.copysign(original_sign)
    }
}

/// Creates a lookup table for an ICC `curv` tag from a transfer function.
///
/// This function generates a vector of 16-bit integers representing the response
/// of the HLG or PQ electro-optical transfer functions (EOTF).
///
/// ### Arguments
/// * `n` - The number of entries in the lookup table. Must not exceed 4096.
/// * `tf` - The transfer function to model, either `TransferFunction::HLG` or `TransferFunction::PQ`.
/// * `tone_map` - A boolean to enable tone mapping for PQ curves. Currently a stub.
///
/// ### Returns
/// A `Result` containing the `Vec<f32>` lookup table or an `Error`.
pub fn create_table_curve(
    n: usize,
    tf: TransferFunction,
    tone_map: bool,
) -> Result<Vec<f32>, Error> {
    // ICC Specification (v4.4, section 10.6) for `curveType` with `curv`
    // processing elements states the table can have at most 4096 entries.
    if n > 4096 {
        return Err(Error::IccTableSizeExceeded(n));
    }

    if !matches!(tf, TransferFunction::PQ | TransferFunction::HLG) {
        return Err(Error::IccUnsupportedTransferFunction);
    }

    // The peak luminance for PQ decoding, as specified in the original C++ code.
    const PQ_INTENSITY_TARGET: f64 = 10000.0;
    // The target peak luminance for SDR, used if tone mapping is applied.
    const DEFAULT_INTENSITY_TARGET: f64 = 255.0; // Placeholder value

    let mut table = Vec::with_capacity(n);
    for i in 0..n {
        // `x` represents the normalized input signal, from 0.0 to 1.0.
        let x = i as f64 / (n - 1) as f64;

        // Apply the specified EOTF to get the linear light value `y`.
        // The output `y` is normalized to the range [0.0, 1.0].
        let y = match tf {
            TransferFunction::HLG => TF_HLG::display_from_encoded(x),
            TransferFunction::PQ => {
                // For PQ, the output of the EOTF is absolute luminance, so we
                // normalize it back to [0, 1] relative to the peak luminance.
                display_from_encoded_pq(PQ_INTENSITY_TARGET as f32, x) / PQ_INTENSITY_TARGET
            }
            _ => unreachable!(), // Already checked above.
        };

        // Apply tone mapping if requested.
        if tone_map && tf == TransferFunction::PQ && PQ_INTENSITY_TARGET > DEFAULT_INTENSITY_TARGET
        {
            // TODO(firsching): add tone mapping here. (make y mutable for this)
            // let linear_luminance = y * PQ_INTENSITY_TARGET;
            // let tone_mapped_luminance = rec2408_tone_map(linear_luminance)?;
            // y = tone_mapped_luminance / DEFAULT_INTENSITY_TARGET;
        }

        // Clamp the final value to the valid range [0.0, 1.0]. This is
        // particularly important for HLG, which can exceed 1.0.
        let y_clamped = y.clamp(0.0, 1.0);

        // table.push((y_clamped * 65535.0).round() as u16);
        table.push(y_clamped as f32);
    }

    Ok(table)
}

#[derive(UnconditionalCoder, Debug, Clone)]
#[validate]
pub struct ColorEncoding {
    #[all_default]
    // TODO(firsching): remove once we use this!
    #[allow(dead_code)]
    all_default: bool,
    #[default(false)]
    pub want_icc: bool,
    #[default(ColorSpace::RGB)]
    pub color_space: ColorSpace,
    #[condition(!want_icc && color_space != ColorSpace::XYB)]
    #[default(WhitePoint::D65)]
    pub white_point: WhitePoint,
    // TODO(veluca): can this be merged in the enum?!
    #[condition(white_point == WhitePoint::Custom)]
    #[default(CustomXY::default(&field_nonserialized))]
    pub white: CustomXY,
    #[condition(!want_icc && color_space != ColorSpace::XYB && color_space != ColorSpace::Gray)]
    #[default(Primaries::SRGB)]
    pub primaries: Primaries,
    #[condition(primaries == Primaries::Custom)]
    #[default([CustomXY::default(&field_nonserialized), CustomXY::default(&field_nonserialized), CustomXY::default(&field_nonserialized)])]
    pub custom_primaries: [CustomXY; 3],
    #[condition(!want_icc)]
    #[default(CustomTransferFunction::default(&field_nonserialized))]
    #[nonserialized(color_space: color_space)]
    pub tf: CustomTransferFunction,
    #[condition(!want_icc)]
    #[default(RenderingIntent::Relative)]
    pub rendering_intent: RenderingIntent,
}

impl ColorEncoding {
    pub fn check(&self, _: &Empty) -> Result<(), Error> {
        if !self.want_icc
            && (self.color_space == ColorSpace::Unknown
                || self.tf.transfer_function == TransferFunction::Unknown)
        {
            Err(Error::InvalidColorEncoding)
        } else {
            Ok(())
        }
    }

    pub fn get_resolved_white_point_xy(&self) -> Result<(f32, f32), Error> {
        let custom_data_for_wp = if self.white_point == WhitePoint::Custom {
            Some(&self.white)
        } else {
            None
        };
        self.white_point.to_xy_coords(custom_data_for_wp)
    }

    pub fn get_resolved_primaries_xy(&self) -> Result<[(f32, f32); 3], Error> {
        let custom_data = if self.primaries == Primaries::Custom {
            Some(&self.custom_primaries)
        } else {
            None
        };
        self.primaries.to_xy_coords(custom_data)
    }

    fn create_icc_cicp_tag_data(&self, tags_data: &mut Vec<u8>) -> Result<Option<TagInfo>, Error> {
        if self.color_space != ColorSpace::RGB {
            return Ok(None);
        }

        // Determine the CICP value for primaries.
        let primaries_val: u8 = if self.primaries == Primaries::P3 {
            if self.white_point == WhitePoint::D65 {
                12 // P3 D65
            } else if self.white_point == WhitePoint::DCI {
                11 // P3 DCI
            } else {
                return Ok(None);
            }
        } else if self.primaries != Primaries::Custom && self.white_point == WhitePoint::D65 {
            // These JXL enum values match the ones for CICP with a D65 white point.
            self.primaries as u8
        } else {
            return Ok(None);
        };

        // Custom gamma or unknown transfer functions cannot be represented.
        if self.tf.have_gamma || self.tf.transfer_function == TransferFunction::Unknown {
            return Ok(None);
        }
        let tf_val = self.tf.transfer_function as u8;

        let signature = b"cicp";
        let start_offset = tags_data.len() as u32;
        tags_data.extend_from_slice(signature);
        let data_len = tags_data.len();
        tags_data.resize(tags_data.len() + 4, 0);
        write_u32_be(tags_data, data_len, 0)?;
        tags_data.push(primaries_val);
        tags_data.push(tf_val);
        // Matrix Coefficients (RGB is non-constant luminance)
        tags_data.push(0);
        // Video Full Range Flag
        tags_data.push(1);

        Ok(Some(TagInfo {
            signature: *signature,
            offset_in_tags_blob: start_offset,
            size_unpadded: 12,
        }))
    }

    fn can_tone_map_for_icc(&self) -> bool {
        // This function determines if an ICC profile can be used for tone mapping.
        // The logic is ported from the libjxl `CanToneMap` function.
        // The core idea is that if the color space can be represented by a CICP tag
        // in the ICC profile, then there's more freedom to use other parts of the
        // profile (like the A2B0 LUT) for tone mapping. Otherwise, the profile must
        // unambiguously describe the color space.

        let tf = self.tf.transfer_function;
        let p = self.primaries;
        let wp = self.white_point;

        // The conditions for being able to tone map are:
        // 1. The color space must be RGB.
        // 2. The transfer function must be either PQ (Perceptual Quantizer) or HLG (Hybrid Log-Gamma).
        // 3. The combination of primaries and white point must be one that is commonly
        //    describable by a standard CICP value. This includes:
        //    a) P3 primaries with either a D65 or DCI white point.
        //    b) Any non-custom primaries, as long as the white point is D65.

        self.color_space == ColorSpace::RGB
            && (tf == TransferFunction::PQ || tf == TransferFunction::HLG)
            && ((p == Primaries::P3 && (wp == WhitePoint::D65 || wp == WhitePoint::DCI))
                || (p != Primaries::Custom && wp == WhitePoint::D65))
    }

    pub fn get_color_encoding_description(&self) -> String {
        // Helper for formatting custom XY float values.
        // Your CustomXY stores i32, which are float * 1_000_000.
        let format_xy_float = |val: i32| -> String { format!("{:.7}", val as f64 / 1_000_000.0) };
        // Helper for formatting gamma float value.
        let format_gamma_float = |val: f32| -> String { format!("{val:.7}") };

        // Handle special known color spaces first
        if self.color_space == ColorSpace::RGB && self.white_point == WhitePoint::D65 {
            if self.rendering_intent == RenderingIntent::Perceptual
                && !self.tf.have_gamma
                && self.tf.transfer_function == TransferFunction::SRGB
            {
                if self.primaries == Primaries::SRGB {
                    return "sRGB".to_string();
                }
                if self.primaries == Primaries::P3 {
                    return "DisplayP3".to_string();
                }
            }
            if self.rendering_intent == RenderingIntent::Relative
                && self.primaries == Primaries::BT2100
            {
                if !self.tf.have_gamma && self.tf.transfer_function == TransferFunction::PQ {
                    return "Rec2100PQ".to_string();
                }
                if !self.tf.have_gamma && self.tf.transfer_function == TransferFunction::HLG {
                    return "Rec2100HLG".to_string();
                }
            }
        }

        // Build the string part by part for other case
        let mut d = String::with_capacity(64);

        // Append ColorSpace string
        d.push_str(&self.color_space.to_string());

        let explicit_wp_tf = self.color_space != ColorSpace::XYB;

        if explicit_wp_tf {
            d.push('_');
            if self.white_point == WhitePoint::Custom {
                // For Custom, we append the specific xy values
                d.push_str(&format_xy_float(self.white.x));
                d.push(';');
                d.push_str(&format_xy_float(self.white.y));
            } else {
                d.push_str(&self.white_point.to_string());
            }
        }

        if self.color_space != ColorSpace::Gray && self.color_space != ColorSpace::XYB {
            d.push('_');
            if self.primaries == Primaries::Custom {
                // For Custom, append specific r,g,b xy values
                // Red primaries
                d.push_str(&format_xy_float(self.custom_primaries[0].x));
                d.push(';');
                d.push_str(&format_xy_float(self.custom_primaries[0].y));
                d.push(';');
                // Green primaries
                d.push_str(&format_xy_float(self.custom_primaries[1].x));
                d.push(';');
                d.push_str(&format_xy_float(self.custom_primaries[1].y));
                d.push(';');
                // Blue primaries
                d.push_str(&format_xy_float(self.custom_primaries[2].x));
                d.push(';');
                d.push_str(&format_xy_float(self.custom_primaries[2].y));
            } else {
                d.push_str(&self.primaries.to_string());
            }
        }

        d.push('_');
        d.push_str(&self.rendering_intent.to_string());

        if explicit_wp_tf {
            d.push('_');
            if self.tf.have_gamma {
                d.push('g');
                d.push_str(&format_gamma_float(self.tf.gamma()));
            } else {
                d.push_str(&self.tf.transfer_function.to_string());
            }
        }
        d
    }

    pub fn create_icc_header(&self) -> Result<Vec<u8>, Error> {
        let mut header_data = vec![0u8; 128];

        // Profile size - To be filled in at the end of profile creation.
        write_u32_be(&mut header_data, 0, 0)?;
        const CMM_TAG: &str = "jxl ";
        // CMM Type
        write_icc_tag(&mut header_data, 4, CMM_TAG)?;

        // Profile version - ICC v4.4 (0x04400000)
        // Conformance tests have v4.3, libjxl produces v4.4
        write_u32_be(&mut header_data, 8, 0x04400000u32)?;

        let profile_class_str = match self.color_space {
            ColorSpace::XYB => "scnr",
            _ => "mntr",
        };
        write_icc_tag(&mut header_data, 12, profile_class_str)?;

        // Data color space
        let data_color_space_str = match self.color_space {
            ColorSpace::Gray => "GRAY",
            _ => "RGB ",
        };
        write_icc_tag(&mut header_data, 16, data_color_space_str)?;

        // PCS - Profile Connection Space
        // Corresponds to: if (kEnable3DToneMapping && CanToneMap(c))
        // Assuming kEnable3DToneMapping is true for this port for now.
        const K_ENABLE_3D_ICC_TONEMAPPING: bool = true;
        if K_ENABLE_3D_ICC_TONEMAPPING && self.can_tone_map_for_icc() {
            write_icc_tag(&mut header_data, 20, "Lab ")?;
        } else {
            write_icc_tag(&mut header_data, 20, "XYZ ")?;
        }

        // Date and Time - Placeholder values from libjxl
        write_u16_be(&mut header_data, 24, 2019)?; // Year
        write_u16_be(&mut header_data, 26, 12)?; // Month
        write_u16_be(&mut header_data, 28, 1)?; // Day
        write_u16_be(&mut header_data, 30, 0)?; // Hours
        write_u16_be(&mut header_data, 32, 0)?; // Minutes
        write_u16_be(&mut header_data, 34, 0)?; // Seconds

        write_icc_tag(&mut header_data, 36, "acsp")?;
        write_icc_tag(&mut header_data, 40, "APPL")?;

        // Profile flags
        write_u32_be(&mut header_data, 44, 0)?;
        // Device manufacturer
        write_u32_be(&mut header_data, 48, 0)?;
        // Device model
        write_u32_be(&mut header_data, 52, 0)?;
        // Device attributes
        write_u32_be(&mut header_data, 56, 0)?;
        write_u32_be(&mut header_data, 60, 0)?;

        // Rendering Intent
        write_u32_be(&mut header_data, 64, self.rendering_intent as u32)?;

        // Whitepoint is fixed to D50 for ICC.
        write_u32_be(&mut header_data, 68, 0x0000F6D6)?;
        write_u32_be(&mut header_data, 72, 0x00010000)?;
        write_u32_be(&mut header_data, 76, 0x0000D32D)?;

        // Profile Creator
        write_icc_tag(&mut header_data, 80, CMM_TAG)?;

        // Profile ID (MD5 checksum) (offset 84) - 16 bytes.
        // This is calculated at the end of profile creation and written here.

        // Reserved (offset 100-127) - already zeroed here.

        Ok(header_data)
    }

    pub fn maybe_create_profile(&self) -> Result<Option<Vec<u8>>, Error> {
        // TODO can reuse `check` above? or at least simplify logic/dedup somehow?
        if self.color_space == ColorSpace::Unknown
            || self.tf.transfer_function == TransferFunction::Unknown
        {
            return Ok(None);
        }
        if !matches!(
            self.color_space,
            ColorSpace::RGB | ColorSpace::Gray | ColorSpace::XYB
        ) {
            return Err(Error::InvalidColorSpace);
        }

        if self.color_space == ColorSpace::XYB
            && self.rendering_intent != RenderingIntent::Perceptual
        {
            return Err(Error::InvalidRenderingIntent);
        }
        let header = self.create_icc_header()?;
        let mut tags_data: Vec<u8> = Vec::new();
        let mut collected_tags: Vec<TagInfo> = Vec::new();

        // Create 'desc' (ProfileDescription) tag
        let description_string = self.get_color_encoding_description();

        let desc_tag_start_offset = tags_data.len() as u32; // 0 at this point ...
        create_icc_mluc_tag(&mut tags_data, &description_string)?;
        let desc_tag_unpadded_size = (tags_data.len() as u32) - desc_tag_start_offset;
        pad_to_4_byte_boundary(&mut tags_data);
        collected_tags.push(TagInfo {
            signature: *b"desc",
            offset_in_tags_blob: desc_tag_start_offset,
            size_unpadded: desc_tag_unpadded_size,
        });

        // Create 'cprt' (Copyright) tag
        let copyright_string = "CC0";
        let cprt_tag_start_offset = tags_data.len() as u32;
        create_icc_mluc_tag(&mut tags_data, copyright_string)?;
        let cprt_tag_unpadded_size = (tags_data.len() as u32) - cprt_tag_start_offset;
        pad_to_4_byte_boundary(&mut tags_data);
        collected_tags.push(TagInfo {
            signature: *b"cprt",
            offset_in_tags_blob: cprt_tag_start_offset,
            size_unpadded: cprt_tag_unpadded_size,
        });

        match self.color_space {
            ColorSpace::Gray => {
                let (wx, wy) = self.get_resolved_white_point_xy()?;
                collected_tags.push(create_icc_xyz_tag(
                    &mut tags_data,
                    &cie_xyz_from_white_cie_xy(wx, wy)?,
                )?);
            }
            _ => {
                // Ok, in this case we will add the chad tag below
                const D50: [f32; 3] = [0.964203f32, 1.0, 0.824905];
                collected_tags.push(create_icc_xyz_tag(&mut tags_data, &D50)?);
            }
        }
        pad_to_4_byte_boundary(&mut tags_data);
        if self.color_space != ColorSpace::Gray {
            let (wx, wy) = self.get_resolved_white_point_xy()?;
            let chad_matrix_f64 = adapt_to_xyz_d50(wx, wy)?;
            let chad_matrix = std::array::from_fn(|r_idx| {
                std::array::from_fn(|c_idx| chad_matrix_f64[r_idx][c_idx] as f32)
            });
            collected_tags.push(create_icc_chad_tag(&mut tags_data, &chad_matrix)?);
            pad_to_4_byte_boundary(&mut tags_data);
        }

        if self.color_space == ColorSpace::RGB {
            if let Some(tag_info) = self.create_icc_cicp_tag_data(&mut tags_data)? {
                collected_tags.push(tag_info);
                // Padding here not necessary, since we add 12 bytes to already 4-byte aligned
                // buffer
                // pad_to_4_byte_boundary(&mut tags_data);
            }

            // Get colorant and white point coordinates to build the conversion matrix.
            let primaries_coords = self.get_resolved_primaries_xy()?;
            let (rx, ry) = primaries_coords[0];
            let (gx, gy) = primaries_coords[1];
            let (bx, by) = primaries_coords[2];
            let (wx, wy) = self.get_resolved_white_point_xy()?;

            // Calculate the RGB to XYZD50 matrix.
            let m = create_icc_rgb_matrix(rx, ry, gx, gy, bx, by, wx, wy)?;

            // Extract the columns, which are the XYZ values for the R, G, and B primaries.
            let r_xyz = [m[0][0], m[1][0], m[2][0]];
            let g_xyz = [m[0][1], m[1][1], m[2][1]];
            let b_xyz = [m[0][2], m[1][2], m[2][2]];

            // Helper to create the raw data for any 'XYZ ' type tag.
            let create_xyz_type_tag_data =
                |tags: &mut Vec<u8>, xyz: &[f32; 3]| -> Result<u32, Error> {
                    let start_offset = tags.len();
                    // The tag *type* is 'XYZ ' for all three
                    tags.extend_from_slice(b"XYZ ");
                    tags.extend_from_slice(&0u32.to_be_bytes());
                    for &val in xyz {
                        append_s15_fixed_16(tags, val)?;
                    }
                    Ok((tags.len() - start_offset) as u32)
                };

            // Create the 'rXYZ' tag.
            let r_xyz_tag_start_offset = tags_data.len() as u32;
            let r_xyz_tag_unpadded_size = create_xyz_type_tag_data(&mut tags_data, &r_xyz)?;
            pad_to_4_byte_boundary(&mut tags_data);
            collected_tags.push(TagInfo {
                signature: *b"rXYZ", // Making the *signature* is unique.
                offset_in_tags_blob: r_xyz_tag_start_offset,
                size_unpadded: r_xyz_tag_unpadded_size,
            });

            // Create the 'gXYZ' tag.
            let g_xyz_tag_start_offset = tags_data.len() as u32;
            let g_xyz_tag_unpadded_size = create_xyz_type_tag_data(&mut tags_data, &g_xyz)?;
            pad_to_4_byte_boundary(&mut tags_data);
            collected_tags.push(TagInfo {
                signature: *b"gXYZ",
                offset_in_tags_blob: g_xyz_tag_start_offset,
                size_unpadded: g_xyz_tag_unpadded_size,
            });

            // Create the 'bXYZ' tag.
            let b_xyz_tag_start_offset = tags_data.len() as u32;
            let b_xyz_tag_unpadded_size = create_xyz_type_tag_data(&mut tags_data, &b_xyz)?;
            pad_to_4_byte_boundary(&mut tags_data);
            collected_tags.push(TagInfo {
                signature: *b"bXYZ",
                offset_in_tags_blob: b_xyz_tag_start_offset,
                size_unpadded: b_xyz_tag_unpadded_size,
            });
        }
        if self.color_space == ColorSpace::XYB {
            todo!("implement A2B0 and B2A0 tags")
        } else if self.can_tone_map_for_icc() {
            todo!("implement A2B0 and B2A0 tags when being able to tone map")
        } else {
            let trc_tag_start_offset = tags_data.len() as u32;
            let trc_tag_unpadded_size = if self.tf.have_gamma {
                // Type 0 parametric curve: Y = X^gamma
                let gamma = 1.0 / self.tf.gamma();
                create_icc_curv_para_tag(&mut tags_data, &[gamma], 0)?
            } else {
                match self.tf.transfer_function {
                    TransferFunction::SRGB => {
                        // Type 3 parametric curve for sRGB standard.
                        const PARAMS: [f32; 5] =
                            [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045];
                        create_icc_curv_para_tag(&mut tags_data, &PARAMS, 3)?
                    }
                    TransferFunction::BT709 => {
                        // Type 3 parametric curve for BT.709 standard.
                        const PARAMS: [f32; 5] =
                            [1.0 / 0.45, 1.0 / 1.099, 0.099 / 1.099, 1.0 / 4.5, 0.081];
                        create_icc_curv_para_tag(&mut tags_data, &PARAMS, 3)?
                    }
                    TransferFunction::Linear => {
                        // Type 3 can also represent a linear response (gamma=1.0).
                        const PARAMS: [f32; 5] = [1.0, 1.0, 0.0, 1.0, 0.0];
                        create_icc_curv_para_tag(&mut tags_data, &PARAMS, 3)?
                    }
                    TransferFunction::DCI => {
                        // Type 3 can also represent a pure power curve (gamma=2.6).
                        const PARAMS: [f32; 5] = [2.6, 1.0, 0.0, 1.0, 0.0];
                        create_icc_curv_para_tag(&mut tags_data, &PARAMS, 3)?
                    }
                    TransferFunction::HLG | TransferFunction::PQ => {
                        let params = create_table_curve(64, self.tf.transfer_function, false)?;
                        create_icc_curv_para_tag(&mut tags_data, params.as_slice(), 3)?
                    }
                    TransferFunction::Unknown => {
                        // This should have been caught by the initial check.
                        return Err(Error::InvalidColorEncoding);
                    }
                }
            };
            pad_to_4_byte_boundary(&mut tags_data);

            match self.color_space {
                ColorSpace::Gray => {
                    // Grayscale profiles use a single 'kTRC' tag.
                    collected_tags.push(TagInfo {
                        signature: *b"kTRC",
                        offset_in_tags_blob: trc_tag_start_offset,
                        size_unpadded: trc_tag_unpadded_size,
                    });
                }
                _ => {
                    // For RGB, rTRC, gTRC, and bTRC all point to the same curve data,
                    // an optimization to keep the profile size small.
                    collected_tags.push(TagInfo {
                        signature: *b"rTRC",
                        offset_in_tags_blob: trc_tag_start_offset,
                        size_unpadded: trc_tag_unpadded_size,
                    });
                    collected_tags.push(TagInfo {
                        signature: *b"gTRC",
                        offset_in_tags_blob: trc_tag_start_offset, // Same offset
                        size_unpadded: trc_tag_unpadded_size,      // Same size
                    });
                    collected_tags.push(TagInfo {
                        signature: *b"bTRC",
                        offset_in_tags_blob: trc_tag_start_offset, // Same offset
                        size_unpadded: trc_tag_unpadded_size,      // Same size
                    });
                }
            }
        }

        // Construct the Tag Table bytes
        let mut tag_table_bytes: Vec<u8> = Vec::new();
        // First, the number of tags (u32)
        tag_table_bytes.extend_from_slice(&(collected_tags.len() as u32).to_be_bytes());

        let header_size = header.len() as u32;
        // Each entry in the tag table on disk is 12 bytes: signature (4), offset (4), size (4)
        let tag_table_on_disk_size = 4 + (collected_tags.len() as u32 * 12);

        for tag_info in &collected_tags {
            tag_table_bytes.extend_from_slice(&tag_info.signature);
            // The offset in the tag table is absolute from the start of the ICC profile file
            let final_profile_offset_for_tag =
                header_size + tag_table_on_disk_size + tag_info.offset_in_tags_blob;
            tag_table_bytes.extend_from_slice(&final_profile_offset_for_tag.to_be_bytes());
            // In https://www.color.org/specification/ICC.1-2022-05.pdf, section 7.3.5 reads:
            //
            // "The value of the tag data element size shall be the number of actual data
            // bytes and shall not include any padding at the end of the tag data element."
            //
            // The reference from conformance tests and libjxl use the padded size here instead.

            tag_table_bytes.extend_from_slice(&tag_info.size_unpadded.to_be_bytes());
            // In order to get byte_exact the same output as libjxl, remove the line above
            // and uncomment the lines below
            // let padded_size = tag_info.size_unpadded.next_multiple_of(4);
            // tag_table_bytes.extend_from_slice(&padded_size.to_be_bytes());
        }

        // Assemble the final ICC profile parts: header + tag_table + tags_data
        let mut final_icc_profile_data: Vec<u8> =
            Vec::with_capacity(header.len() + tag_table_bytes.len() + tags_data.len());
        final_icc_profile_data.extend_from_slice(&header);
        final_icc_profile_data.extend_from_slice(&tag_table_bytes);
        final_icc_profile_data.extend_from_slice(&tags_data);

        // Update the profile size in the header (at offset 0)
        let total_profile_size = final_icc_profile_data.len() as u32;
        write_u32_be(&mut final_icc_profile_data, 0, total_profile_size)?;

        // Assemble the final ICC profile parts: header + tag_table + tags_data
        let mut final_icc_profile_data: Vec<u8> =
            Vec::with_capacity(header.len() + tag_table_bytes.len() + tags_data.len());
        final_icc_profile_data.extend_from_slice(&header);
        final_icc_profile_data.extend_from_slice(&tag_table_bytes);
        final_icc_profile_data.extend_from_slice(&tags_data);

        // Update the profile size in the header (at offset 0)
        let total_profile_size = final_icc_profile_data.len() as u32;
        write_u32_be(&mut final_icc_profile_data, 0, total_profile_size)?;

        // The MD5 checksum (Profile ID) must be computed on the profile with
        // specific header fields zeroed out, as per the ICC specification.
        let mut profile_for_checksum = final_icc_profile_data.clone();

        if profile_for_checksum.len() >= 84 {
            // Zero out Profile Flags at offset 44.
            profile_for_checksum[44..48].fill(0);
            // Zero out Rendering Intent at offset 64.
            profile_for_checksum[64..68].fill(0);
            // The Profile ID field at offset 84 is already zero at this stage.
        }

        // Compute the MD5 hash on the modified profile data.
        let mut context = Context::new();
        context.consume(&profile_for_checksum);
        let checksum = *context.compute();

        // Write the 16-byte checksum into the "Profile ID" field of the *original*
        // profile data buffer, starting at offset 84.
        if final_icc_profile_data.len() >= 100 {
            final_icc_profile_data[84..100].copy_from_slice(&checksum);
        }

        Ok(Some(final_icc_profile_data))
    }

    pub fn srgb(grayscale: bool) -> Self {
        let color_space = if grayscale {
            ColorSpace::Gray
        } else {
            ColorSpace::RGB
        };
        let mut result = Self::default(&Empty {});
        result.color_space = color_space;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_matrix_eq(a: &Matrix3x3<f64>, b: &Matrix3x3<f64>, epsilon: f64) {
        for r in 0..3 {
            for c in 0..3 {
                assert!(
                    (a[r][c] - b[r][c]).abs() < epsilon,
                    "Matrices differ at [{}][{}]: expected {}, got {}. Diff: {}",
                    r,
                    c,
                    b[r][c],
                    a[r][c],
                    (a[r][c] - b[r][c]).abs()
                );
            }
        }
    }

    #[test]
    fn test_3x3_inverse() {
        // Random matrix (https://xkcd.com/221/)
        let m: Matrix3x3<f64> = [[1.0f64, -3.0, -2.0], [2.0, 2.0, 1.0], [2.0, 1.0, 1.0]];

        let expected_inv: Matrix3x3<f64> = [[0.2, 0.2, 0.2], [0., 1., -1.], [-0.4, -1.4, 1.6]];

        match inv_3x3_matrix(&m) {
            Ok(inv_m) => {
                assert_matrix_eq(&inv_m, &expected_inv, 1e-12);
            }
            Err(e) => {
                panic!("Matrix inversion failed unexpectedly: {e:?}");
            }
        }
    }

    #[test]
    fn test_srgb() {
        assert_eq!(
            ColorEncoding::srgb(false).get_color_encoding_description(),
            "RGB_D65_SRG_Rel_SRG"
        );
        assert_eq!(
            ColorEncoding::srgb(true).get_color_encoding_description(),
            "Gra_D65_Rel_SRG"
        );
    }
}
