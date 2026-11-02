use ego_tree::NodeRef;
use ego_tree::iter::Edge;
use scraper::Node;

/// 줄바꿈으로 취급하는 태그
const BLOCK_TAGS: [&str; 7] = ["br", "div", "p", "li", "ul", "tr", "table"];

/// 블록 태그 경계를 줄바꿈으로 바꿔 노드의 텍스트를 이어 붙입니다.
pub(crate) fn block_text(node: NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    for edge in node.traverse() {
        match edge {
            Edge::Open(n) => match n.value() {
                Node::Text(text) => out.push_str(&text.text),
                Node::Element(el) if BLOCK_TAGS.contains(&el.name()) => out.push('\n'),
                _ => {}
            },
            Edge::Close(n) => {
                if let Node::Element(el) = n.value()
                    && BLOCK_TAGS.contains(&el.name())
                {
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// 연속 공백을 하나로 줄이고 앞뒤 공백을 지웁니다.
pub(crate) fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 빈 줄을 버리고 각 줄의 공백을 정리합니다.
pub(crate) fn lines(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split('\n')
        .map(collapse_whitespace)
        .filter(|line| !line.is_empty())
}
