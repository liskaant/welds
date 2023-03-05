use super::clause::{DbParam, NextParam};
use crate::errors::Result;
use crate::query::clause::ClauseAdder;
use crate::table::{TableColumns, TableInfo};
use crate::writers::column::{ColumnWriter, DbColumnWriter};
use crate::writers::limit_skip::{DbLimitSkipWriter, LimitSkipWriter};
use sqlx::database::HasArguments;
use sqlx::query::{Query, QueryAs};
use sqlx::IntoArguments;
use sqlx::Row;
use std::marker::PhantomData;

pub struct SelectBuilder<'schema, T, S, DB: sqlx::Database> {
    _t: PhantomData<T>,
    _s: PhantomData<S>,
    wheres: Vec<Box<dyn ClauseAdder<'schema, DB>>>,
    limit: Option<i64>,
    offset: Option<i64>,
    // This is needed for lifetime issues, remove if you can
    history: Vec<String>,
}

impl<'schema, 'args, T, S, DB> SelectBuilder<'schema, T, S, DB>
where
    DB: sqlx::Database,
    S: Default + TableInfo + TableColumns<DB>,
    T: Send + Unpin + for<'r> sqlx::FromRow<'r, DB::Row>,
{
    pub fn new() -> Self {
        Self {
            _t: Default::default(),
            _s: Default::default(),
            wheres: Vec::default(),
            limit: None,
            offset: None,
            history: Default::default(),
        }
    }

    pub fn where_col(mut self, lam: impl Fn(S) -> Box<dyn ClauseAdder<'schema, DB>>) -> Self {
        let c = S::default();
        let qba = lam(c);
        self.wheres.push(qba);
        self
    }

    pub fn limit(mut self, x: i64) -> Self {
        self.limit = Some(x);
        self
    }

    pub fn offset(mut self, x: i64) -> Self {
        self.offset = Some(x);
        self
    }

    pub fn to_sql(&mut self) -> String
    where
        <DB as HasArguments<'schema>>::Arguments: IntoArguments<'args, DB>,
        DB: DbParam + DbColumnWriter + DbLimitSkipWriter,
    {
        let mut args: Option<<DB as HasArguments>::Arguments> = None;
        let next_params = NextParam::new::<DB>();
        let wheres = self.wheres.as_slice();

        join_sql_parts(&[
            build_head_select::<DB, S>(),
            build_where(&next_params, &mut args, wheres),
            build_tail(&self),
        ])
    }

    pub async fn count<'q, 'e, E>(&'q mut self, exec: E) -> Result<u64>
    where
        'q: 'args,
        E: sqlx::Executor<'e, Database = DB>,
        <DB as HasArguments<'schema>>::Arguments: IntoArguments<'args, DB>,
        i64: sqlx::Type<DB> + for<'r> sqlx::Decode<'r, DB>,
        usize: sqlx::ColumnIndex<<DB as sqlx::Database>::Row>,
        DB: DbParam + DbColumnWriter + DbLimitSkipWriter,
    {
        let mut args: Option<<DB as HasArguments>::Arguments> = Some(Default::default());
        let next_params = NextParam::new::<DB>();
        let wheres = self.wheres.as_slice();

        let sql = join_sql_parts(&[
            build_head_count::<DB, S>(),
            build_where(&next_params, &mut args, wheres),
            build_tail(&self),
        ]);

        // lifetime hack
        self.history.push(sql);
        let sql = self.history.last().unwrap();

        eprintln!("RUNNING: {}", sql);

        // Run the query
        let query: Query<DB, <DB as HasArguments>::Arguments> =
            sqlx::query_with(sql, args.unwrap());
        let row = query.fetch_one(exec).await?;
        let count: i64 = row.try_get(0)?;
        Ok(count as u64)
    }

    pub async fn run<'q, 'e, E>(&'q mut self, exec: E) -> Result<Vec<T>>
    where
        'q: 'args,
        E: sqlx::Executor<'e, Database = DB>,
        <DB as HasArguments<'schema>>::Arguments: IntoArguments<'args, DB>,
        DB: DbParam + DbColumnWriter + DbLimitSkipWriter,
    {
        let mut args: Option<<DB as HasArguments>::Arguments> = Some(Default::default());
        let next_params = NextParam::new::<DB>();
        let wheres = self.wheres.as_slice();

        let sql = join_sql_parts(&[
            build_head_select::<DB, S>(),
            build_where(&next_params, &mut args, wheres),
            build_tail(&self),
        ]);

        // lifetime hack
        self.history.push(sql);
        let sql = self.history.last().unwrap();

        // Run the query
        let q: QueryAs<DB, T, <DB as HasArguments>::Arguments> =
            sqlx::query_as_with(sql, args.unwrap());
        let data = q.fetch_all(exec).await?;
        Ok(data)
    }
}

fn join_sql_parts(parts: &[Option<String>]) -> String {
    // Join al the parts into
    let sql: Vec<&str> = parts
        .iter()
        .filter_map(|x| x.as_ref().map(|x| x.as_str()))
        .collect();
    let sql: String = sql.as_slice().join(" ");
    sql
}

fn build_where<'schema, 'args, DB>(
    next_params: &NextParam,
    args: &mut Option<<DB as HasArguments<'schema>>::Arguments>,
    wheres: &[Box<dyn ClauseAdder<'schema, DB>>],
) -> Option<String>
where
    DB: sqlx::Database,
    <DB as HasArguments<'schema>>::Arguments: IntoArguments<'args, DB>,
{
    if wheres.len() == 0 {
        return None;
    }
    let mut where_sql: Vec<String> = Vec::default();
    where_sql.push("WHERE".to_owned());
    for clause in wheres {
        if let Some(args) = args {
            clause.bind(args);
        }
        if let Some(p) = clause.clause(&next_params) {
            where_sql.push(p);
        }
    }
    Some(where_sql.join(" "))
}

fn build_tail<'schema, T, S, DB>(select: &SelectBuilder<'schema, T, S, DB>) -> Option<String>
where
    DB: sqlx::Database + DbLimitSkipWriter,
    S: TableInfo + TableColumns<DB>,
{
    if select.limit.is_none() && select.offset.is_none() {
        return None;
    }
    let w = LimitSkipWriter::new::<DB>();
    Some(format!(
        "{} {}",
        w.limit(select.limit),
        w.skip(select.offset)
    ))
}

fn build_head_select<DB, S>() -> Option<String>
where
    DB: sqlx::Database + DbColumnWriter,
    S: TableInfo + TableColumns<DB>,
{
    let writer = ColumnWriter::new::<DB>();
    let mut head: Vec<&str> = Vec::default();
    head.push("SELECT");
    let cols_info = S::columns();
    let cols: Vec<_> = cols_info.iter().map(|col| writer.write(col)).collect();
    let cols = cols.join(", ");
    head.push(&cols);
    head.push("FROM");
    head.push(S::identifier());
    Some(head.join(" "))
}

fn build_head_count<DB, S>() -> Option<String>
where
    DB: sqlx::Database + DbColumnWriter,
    S: TableInfo + TableColumns<DB>,
{
    let mut head: Vec<&str> = Vec::default();
    head.push("SELECT cast(count(*) as bigint) FROM");
    head.push(S::identifier());
    Some(head.join(" "))
}
