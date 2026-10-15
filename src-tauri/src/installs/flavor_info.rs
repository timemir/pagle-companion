//! A flavor folder's `.flavor.info` (CMP Contracts): a Blizzard
//! pipe-separated table whose `Product Flavor` column names the product code
//! of the install (CMP-R12).

/// The column that names the product code, matched exactly.
const PRODUCT_FLAVOR: &str = "Product Flavor";

/// The product code a `.flavor.info` names, or `None` when it names none: no
/// `Product Flavor` column, or no data line.
pub fn product_code(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
        .replace('\r', "");
    let mut lines = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
    let header = lines.next()?;
    let data = lines.next()?;
    let column = header
        .split('|')
        .position(|column| column.split('!').next() == Some(PRODUCT_FLAVOR))?;
    data.split('|').nth(column).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_product_under_product_flavor() {
        // CMP Contracts, .flavor.info: the product code is the first data
        // line's value under Product Flavor
        assert_eq!(
            product_code(b"Product Flavor!STRING:0\nwow_classic_beta\n").as_deref(),
            Some("wow_classic_beta")
        );
    }

    #[test]
    fn a_bom_and_cr_are_stripped() {
        // CMP Contracts, .flavor.info: a UTF-8 byte order mark and CR
        // characters are stripped
        assert_eq!(
            product_code(b"\xEF\xBB\xBFProduct Flavor!STRING:0\r\nwow_classic_beta\r\n").as_deref(),
            Some("wow_classic_beta")
        );
    }

    #[test]
    fn blank_and_comment_lines_are_skipped() {
        // CMP Contracts, .flavor.info: blank lines and lines starting with #
        // are skipped
        let file = b"## seqn = 1\n\nBranch!STRING:0|Product Flavor!STRING:0|Tag!HEX:16\n# note\n\neu|wow_classic_beta|00\nus|wow|01\n";
        assert_eq!(product_code(file).as_deref(), Some("wow_classic_beta"));
    }

    #[test]
    fn a_file_without_the_column_or_a_data_line_names_no_product() {
        // CMP Contracts, .flavor.info: no such column or no data line names
        // no product; the column name is matched exactly
        assert_eq!(product_code(b"Flavor!STRING:0\nwow_classic_beta\n"), None);
        assert_eq!(
            product_code(b"product flavor!STRING:0\nwow_classic_beta\n"),
            None
        );
        assert_eq!(product_code(b"Product Flavor!STRING:0\n"), None);
        assert_eq!(product_code(b""), None);
        assert_eq!(
            product_code(b"A!STRING:0|Product Flavor!STRING:0\nonly-one\n"),
            None
        );
    }

    #[test]
    fn the_value_is_kept_byte_for_byte() {
        // CMP-R12: compared byte for byte, so nothing is trimmed or folded
        assert_eq!(
            product_code(b"Product Flavor!STRING:0\nWoW_Classic_Beta \n").as_deref(),
            Some("WoW_Classic_Beta ")
        );
    }
}
