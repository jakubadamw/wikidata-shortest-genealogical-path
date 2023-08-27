#![feature(result_option_inspect)]

use std::{borrow::Cow, collections::HashMap};

use futures_util::StreamExt;
use itertools::Itertools;
use worker::{
    console_debug, console_error, event, Env, Headers, Request, Response, Result as WorkerResult,
    Router,
};

#[event(start)]
pub fn main() {
    console_error_panic_hook::set_once();
}

const MAX_PATH_LENGTH: usize = 3;

const ENDPOINT: &str = "https://qlever.cs.uni-freiburg.de/api/wikidata";

#[derive(serde::Deserialize)]
struct Result {
    res: Vec<Vec<String>>,
    selected: Vec<String>,
}

#[derive(Clone, serde::Serialize)]
struct Person {
    index: usize,
    name: Option<String>,
    birth_date: Option<chrono::NaiveDate>,
    death_date: Option<chrono::NaiveDate>,
    image_url: Option<String>,
}

#[derive(Clone, serde::Serialize)]
struct Relation {
    from_index: usize,
    to_index: usize,
    relation_type: NormalizedRelationType,
}

#[derive(Clone, Copy, serde::Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RelationType {
    Child,
    Father,
    Mother,
    Spouse,
}

#[derive(Clone, Copy, serde::Serialize, PartialEq, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
enum NormalizedRelationType {
    Parent,
    Spouse,
}

impl TryFrom<u16> for RelationType {
    type Error = ();

    fn try_from(value: u16) -> std::result::Result<Self, Self::Error> {
        match value {
            22 => Ok(Self::Father),
            25 => Ok(Self::Mother),
            26 => Ok(Self::Spouse),
            40 => Ok(Self::Child),
            _ => Err(()),
        }
    }
}

#[derive(serde::Serialize)]
struct Path {
    people: Vec<Person>,
    relations: Vec<Relation>,
}

type Node = Person;
type Edge = Relation;

impl<'a> dot::Labeller<'a, Node, Edge> for Path {
    fn graph_id(&'a self) -> dot::Id<'a> {
        dot::Id::new("G").unwrap()
    }

    fn node_id(&'a self, node: &Node) -> dot::Id<'a> {
        dot::Id::new(format!("N{}", node.index)).unwrap()
    }

    fn node_label(&'a self, node: &Node) -> dot::LabelText<'a> {
        dot::LabelText::LabelStr(node.name.clone().unwrap_or_default().into())
    }

    fn edge_label(&'a self, edge: &Edge) -> dot::LabelText<'a> {
        dot::LabelText::LabelStr(Cow::Borrowed(edge.relation_type.into()))
    }

    /*fn edge_color(&'a self, edge: &Edge) -> Option<dot::LabelText<'a>> {
        Some(dot::LabelText::LabelStr(
            match edge.relation_type {
                NormalizedRelationType::Parent => "deepskyblue",
                NormalizedRelationType::Spouse => "darkseagreen",
            }
            .into(),
        ))
    }*/
}

impl<'a> dot::GraphWalk<'a, Node, Edge> for Path {
    fn nodes(&'a self) -> dot::Nodes<'a, Node> {
        Cow::Borrowed(self.people.as_slice())
    }

    fn edges(&'a self) -> dot::Edges<'a, Edge> {
        Cow::Borrowed(&self.relations)
    }

    fn source(&self, edge: &Edge) -> Node {
        self.people
            .iter()
            .find(|person| person.index == edge.from_index)
            .expect("must exist")
            .clone()
    }

    fn target(&self, edge: &Edge) -> Node {
        self.people
            .iter()
            .find(|person| person.index == edge.to_index)
            .expect("must exist")
            .clone()
    }
}

fn parse_date(string: String) -> Option<chrono::NaiveDate> {
    const DATE_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
    chrono::NaiveDate::parse_from_str(string.split('^').next()?.trim_matches('"'), DATE_FORMAT).ok()
}

async fn get_wikidata_id(
    reqwest_client: &reqwest::Client,
    title: &str,
) -> WorkerResult<Option<String>> {
    use json_dotpath::DotPaths;

    const URL: &str = "https://en.wikipedia.org/w/api.php";

    let mut url: url::Url = URL.parse().expect("must be valid");
    url.query_pairs_mut().append_pair("action", "query");
    url.query_pairs_mut().append_pair("prop", "pageprops");
    url.query_pairs_mut().append_pair("format", "json");
    url.query_pairs_mut().append_pair("titles", title);

    let value: serde_json::Value = reqwest_client
        .get(url)
        .send()
        .await
        .map_err(|err| worker::Error::RustError(err.to_string()))?
        .error_for_status()
        .map_err(|err| worker::Error::RustError(err.to_string()))?
        .json()
        .await
        .map_err(|err| worker::Error::RustError(err.to_string()))?;

    Ok(value
        .dot_get::<serde_json::Value>("query.pages")
        .ok()
        .flatten()
        .into_iter()
        .filter_map(|value| value.as_object().cloned())
        .flatten()
        .collect_tuple()
        .and_then(|((_, object),)| {
            object
                .dot_get::<String>("pageprops.wikibase_item")
                .ok()
                .flatten()
        }))
}

async fn compute_paths(
    reqwest_client: &reqwest::Client,
    from: &str,
    to: &str,
) -> WorkerResult<Option<Path>> {
    use futures_util::TryStreamExt;

    let stream = futures_util::stream::iter(
        (1..=MAX_PATH_LENGTH)
            .map(|path_length| async move {
                let query = construct_query(path_length, &from, &to);
                let Result { selected, res } = send_request(&reqwest_client, &query).await?;
                WorkerResult::Ok(res.into_iter().map(move |row| {
                    selected
                        .clone()
                        .into_iter()
                        .zip(row)
                        .collect::<HashMap<String, String>>()
                }))
            })
            .map(Ok),
    )
    .try_buffered(4)
    .map_ok(|value| futures_util::stream::iter(value).map(WorkerResult::Ok))
    .try_flatten();

    futures_util::pin_mut!(stream);

    Ok(match stream.try_next().await? {
        Some(path) => {
            console_debug!("MAP:\n{path:#?}");

            let (vertex_pairs, edge_pairs) = path
                .into_iter()
                .partition::<Vec<_>, _>(|(key, _)| key.starts_with("?vertex_"));
            let people = vertex_pairs
                .into_iter()
                .filter_map(|(key, value)| {
                    let mut key_split = key.splitn(3, '_');
                    assert_eq!(key_split.next().expect("must exist"), "?vertex");
                    let index: usize = key_split
                        .next()
                        .expect("must exist")
                        .parse()
                        .expect("must be valid");
                    let key = key_split.next()?.to_owned();
                    Some((index, (key, value)))
                })
                .into_group_map();
            let mut people: Vec<_> = people
                .into_iter()
                .map(|(index, properties)| {
                    let mut properties = properties.into_iter().collect::<HashMap<_, _>>();
                    Person {
                        index,
                        name: properties.remove("name").and_then(|value| {
                            value
                                .split('@')
                                .next()
                                .map(|value| value.trim_matches('"').to_owned())
                        }),
                        birth_date: properties.remove("birth_date").and_then(parse_date),
                        death_date: properties.remove("death_date").and_then(parse_date),
                        image_url: properties.remove("image_url").map(|value| {
                            value
                                .trim_start_matches('<')
                                .trim_end_matches('>')
                                .to_owned()
                        }),
                    }
                })
                .collect();
            people.sort_by_key(|person| person.index);
            let relations = edge_pairs
                .into_iter()
                .filter_map(|(key, value)| {
                    let mut key_split = key.splitn(3, '_');
                    assert_eq!(key_split.next().expect("must exist"), "?edge");
                    let from_index = key_split.next()?.to_owned().parse().ok()?;
                    let to_index = key_split.next()?.to_owned().parse().ok()?;
                    let (_, relation_type) = value.rsplit_once('/')?;
                    let relation_type: u16 = relation_type
                        .trim_start_matches('P')
                        .trim_end_matches('>')
                        .parse()
                        .ok()?;
                    let relation_type = relation_type.try_into().ok()?;
                    Some(match relation_type {
                        RelationType::Child => vec![Relation {
                            from_index: to_index,
                            to_index: from_index,
                            relation_type: NormalizedRelationType::Parent,
                        }],
                        RelationType::Mother | RelationType::Father => vec![Relation {
                            from_index,
                            to_index,
                            relation_type: NormalizedRelationType::Parent,
                        }],
                        RelationType::Spouse => vec![
                            Relation {
                                from_index,
                                to_index,
                                relation_type: NormalizedRelationType::Spouse,
                            },
                            Relation {
                                from_index: to_index,
                                to_index: from_index,
                                relation_type: NormalizedRelationType::Spouse,
                            },
                        ],
                    })
                })
                .flatten()
                .collect();
            Some(Path { people, relations })
        }
        None => None,
    })
}

fn construct_query(path_length: usize, from: &str, to: &str) -> String {
    let vertices: Vec<(usize, String)> = (0..=(path_length + 1))
        .map(|index| (index, format!("?vertex_{index:02}")))
        .collect();
    let query = [format!("VALUES ?vertex_00 {{ wd:{from} }}"), format!("VALUES ?vertex_{:02} {{ wd:{to} }}", path_length + 1)]
            .into_iter()
            .chain((0..=(path_length + 1))
                .tuple_windows()
                .map(|(from_vertex_index, to_vertex_index)| {
                    format!(
                        "VALUES ?edge_{from_vertex_index:02}_{to_vertex_index:02} {{ wdt:P22 wdt:P25 wdt:P26 wdt:P40 }}"
                    )
                }
            ))
            .chain(vertices.iter().tuple_windows().map(
                |((from_vertex_index, from), (to_vertex_index, to))| {
                    format!("{from} ?edge_{from_vertex_index:02}_{to_vertex_index:02} {to} .")
                },
            ))
            .chain(
                vertices
                    .iter()
                    .flat_map(|(index, name)| {
                        [
                            format!("OPTIONAL {{ {name} wdt:P569 ?vertex_{index:02}_birth_date }}"),
                            format!("OPTIONAL {{ {name} wdt:P570 ?vertex_{index:02}_death_date }}"),
                            format!("OPTIONAL {{ {name} wdt:P18 ?vertex_{index:02}_image_url }}"),
                            format!("{name} schema:name ?vertex_{index:02}_name ."),
                            format!("FILTER (lang(?vertex_{index:02}_name) = \"en\")"),
                        ]
                    }),
            )
            .map(|line| format!("  {line}"))
            .join("\n");

    format!(
        "\
            PREFIX wd: <http://www.wikidata.org/entity/> \n\
            PREFIX wdt: <http://www.wikidata.org/prop/direct/> \n\
            PREFIX wikibase: <http://wikiba.se/ontology#> \n\
            PREFIX schema: <http://schema.org/> \n\
            SELECT * \n\
            WHERE {{ \n\
            {query} \n\
            }} \n\
        "
    )
}

async fn send_request(reqwest_client: &reqwest::Client, query: &str) -> WorkerResult<Result> {
    let mut params = std::collections::HashMap::new();
    params.insert("query", query.to_owned());
    params.insert("send", 100.to_string());
    let string = reqwest_client
        .post(ENDPOINT)
        .header(reqwest::header::ACCEPT, "application/qlever-results+json")
        .header(reqwest::header::USER_AGENT, "wikidata-test")
        .form(&params)
        .send()
        .await
        .map_err(|err| worker::Error::RustError(err.to_string()))?
        .error_for_status()
        .map_err(|err| worker::Error::RustError(err.to_string()))?
        .text()
        .await
        .map_err(|err| worker::Error::RustError(err.to_string()))?;

    let result: Result = serde_json::from_str(&string)
        .inspect_err(|_| console_error!("Failed to deserialize:\n\n{string}"))
        .map_err(|err| worker::Error::RustError(err.to_string()))?;

    if !result.res.is_empty() {
        assert_eq!(result.selected.len(), result.res[0].len());
    }
    Ok(result)
}

fn render_svg(path: Path) -> WorkerResult<Vec<u8>> {
    use layout::backends::svg::SVGWriter;
    use layout::gv::{DotParser, GraphBuilder};

    let mut dot_contents: Vec<u8> = vec![];
    dot::render(&path, &mut dot_contents).unwrap();
    let dot_contents = String::from_utf8(dot_contents).unwrap();

    worker::console_log!("dot contents:\n{dot_contents}");

    let mut parser = DotParser::new(&dot_contents);

    let graph = parser
        .process()
        .map_err(|err| worker::Error::RustError(err.to_string()))?;
    let mut graph_builder = GraphBuilder::new();
    graph_builder.visit_graph(&graph);
    let mut visual_graph = graph_builder.get();

    let mut svg_writer = SVGWriter::new();
    visual_graph.do_it(false, true, false, &mut svg_writer);
    let svg_contents = svg_writer.finalize();
    Ok(svg_contents.into_bytes())
}

#[event(fetch)]
pub async fn main(req: Request, env: Env, context: worker::Context) -> WorkerResult<Response> {
    let router = Router::with_data(context);
    router
        .get_async("/path", |req, _ctx| async move {
            let reqwest_client = reqwest::Client::new();

            let url = req.url()?;
            let mut query_pairs = url.query_pairs().collect::<HashMap<_, _>>();
            let from = query_pairs.remove("from").expect("must be defined");
            let to = query_pairs.remove("to").expect("must be defined");

            let (from_id, to_id) = futures_util::try_join! {
                get_wikidata_id(&reqwest_client, from.as_ref()),
                get_wikidata_id(&reqwest_client, to.as_ref()),
            }?;
            console_debug!("from ID: {from_id:?}, to ID: {to_id:?}");

            let shortest_path = match from_id.zip(to_id) {
                Some((from_id, to_id)) => compute_paths(&reqwest_client, &from_id, &to_id).await?,
                None => None,
            };

            let mut headers = Headers::new();
            headers.set(reqwest::header::CONTENT_TYPE.as_str(), "image/svg+xml")?;
            match shortest_path {
                Some(shortest_path) => {
                    let svg = render_svg(shortest_path)?;
                    Ok(Response::from_bytes(svg)?.with_headers(headers))
                }
                None => Response::error("", 404),
            }
        })
        .run(req, env)
        .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_construct_query() {
        let query = super::construct_query(1, "Q346", "Q295268");
        insta::assert_snapshot!(query);
    }
}
