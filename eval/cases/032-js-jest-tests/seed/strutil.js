function reverse(s) {
    return s.split("").reverse().join("");
}

function countVowels(s) {
    return (s.match(/[aeiouAEIOU]/g) || []).length;
}

function titleCase(s) {
    return s
        .split(/\s+/)
        .filter(Boolean)
        .map((w) => w[0].toUpperCase() + w.slice(1).toLowerCase())
        .join(" ");
}

module.exports = { reverse, countVowels, titleCase };
