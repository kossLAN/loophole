use std::any::Any;
use std::rc::Rc;

use slint::{Model, ModelRc, ModelTracker, VecModel};

// Slint writes a row back through set_row_data when a two-way binding to one of its fields
// changes. Rust's own writes go to `rows` and do not count as edits. `on_edit` must not borrow
// what owns the model: it runs inside Slint's event handling.
pub struct Tracked<T> {
    rows: VecModel<T>,
    on_edit: Rc<dyn Fn()>,
}

impl<T: Clone + 'static> Tracked<T> {
    pub fn new(on_edit: Rc<dyn Fn()>) -> Rc<Self> {
        Rc::new(Self {
            rows: VecModel::default(),
            on_edit,
        })
    }

    pub fn rows(&self) -> &VecModel<T> {
        &self.rows
    }

    pub fn update(&self, i: usize, f: impl FnOnce(&mut T)) {
        if let Some(mut row) = self.rows.row_data(i) {
            f(&mut row);
            self.rows.set_row_data(i, row);
        }
    }

    pub fn to_vec(&self) -> Vec<T> {
        self.rows.iter().collect()
    }

    pub fn model(self: &Rc<Self>) -> ModelRc<T> {
        ModelRc::from(self.clone() as Rc<dyn Model<Data = T>>)
    }

    pub fn from_model(model: &ModelRc<T>) -> Option<&Self> {
        model.as_any().downcast_ref::<Self>()
    }
}

impl<T: Clone + 'static> Model for Tracked<T> {
    type Data = T;

    fn row_count(&self) -> usize {
        self.rows.row_count()
    }

    fn row_data(&self, row: usize) -> Option<T> {
        self.rows.row_data(row)
    }

    fn set_row_data(&self, row: usize, data: T) {
        self.rows.set_row_data(row, data);
        (self.on_edit)();
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        self.rows.model_tracker()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
