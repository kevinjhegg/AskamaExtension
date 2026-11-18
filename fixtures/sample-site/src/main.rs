//! The extension's test bed. `cargo run` renders the order page to stdout, which proves the templates compile with Askama itself.

use askama::Template;

pub struct Customer {
    pub name: String,
    pub email: String,
}

pub struct LineItem {
    pub sku: String,
    pub quantity: u32,
    pub unit_price_cents: u64,
}

impl LineItem {
    pub fn total_cents(&self) -> u64 {
        self.unit_price_cents * u64::from(self.quantity)
    }
}

pub struct Order {
    pub id: u64,
    pub customer: Customer,
    pub items: Vec<LineItem>,
}

#[derive(Template)]
#[template(path = "order.html")]
pub struct OrderPage {
    pub title: String,
    pub order: Order,
}

mod filters {
    /// Formats an amount in cents as dollars, for example `1250` as `$12.50`.
    #[askama::filter_fn]
    pub fn dollars(cents: &u64, _: &dyn askama::Values) -> askama::Result<String> {
        Ok(format!("${}.{:02}", cents / 100, cents % 100))
    }
}

fn main() {
    let page = OrderPage {
        title: "Order 42".to_owned(),
        order: Order {
            id: 42,
            customer: Customer { name: "Ada".to_owned(), email: "ada@example.com".to_owned() },
            items: vec![
                LineItem { sku: "BOLT-10".to_owned(), quantity: 4, unit_price_cents: 125 },
                LineItem { sku: "NUT-10".to_owned(), quantity: 4, unit_price_cents: 40 },
            ],
        },
    };
    println!("{}", page.render().expect("the order page renders"));
}
