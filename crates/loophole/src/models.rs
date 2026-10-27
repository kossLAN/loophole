use std::any::Any;
use std::rc::Rc;

use slint::{Model, ModelRc, ModelTracker, VecModel};

pub fn sync<T>(model: &VecModel<T>, rows: Vec<T>)
where
    T: Clone + PartialEq + 'static,
{
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }

    for (i, row) in rows.into_iter().enumerate() {
        match model.row_data(i) {
            Some(old) if old == row => {},
            Some(_) => model.set_row_data(i, row),
            None => model.push(row),
        }
    }
}

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
